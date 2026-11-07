"""The link with the earlier command name, the earlier client and the earlier settings folder in `setup.sh` (COMPAT-CAH, hand-edit item setup-06).
# COMPAT-CAH: this file names the earlier command and folder on purpose, and ends with the shim it tests.

`setup.sh` installs the client `ham` and, beside it, a relative link with the name the client had before (`cah`). It replaces an existing file of that name only when the
file is a link to the client or is recognised WITHOUT being run as an earlier client of this system (a line of a setup receipt holds its SHA-256, or it holds the header name
the earlier client reads the backend version from); any other file is kept with a warning. A client on the PATH counts only when it holds the header name the client reads
(`x-hamh-backend-version`). `--from DIR` with only the earlier name exits 1 before anything is installed. The Config step runs `ham config migrate` when only the earlier
settings folder holds settings; it counts a folder of the new place as holding settings by the rule of the client (an `engines`, `rules` or `skills` folder counts only with something
in it besides a file browser's `.DS_Store`, and a copy that stopped half way, marked by `.ham-migrating`, counts for nothing). A client given with `--from` as a file or in a folder is installed only
when it holds the header name the current client reads (`x-hamh-backend-version`): the earlier client does not. Every case runs the real script in a fake HOME with stub commands, as test_setup_script.py does.
"""

from __future__ import annotations

import os
import stat
import unittest
from pathlib import Path

from test_setup_script import STUB_HAM, SetupBase, sha256_of, tree_state

OLD = "cah"  # the earlier command name: a probe word, not a name of this program
HEADER = "x-hamh-backend-version"
OLD_HEADER = "x-cah-backend-version"
# the stand-in client with the header name in it, as the real client has it, and `config migrate` (the real one is the client's command)
CLIENT = STUB_HAM
assert HEADER in CLIENT
MIGRATING = CLIENT.replace('case "$1 $2" in\n', 'case "$1 $2" in\n  "config migrate")\n    mkdir -p "$(dirname "$CFG")"; cp "${XDG_CONFIG_HOME:-$HOME/.config}/' + OLD + '/config.toml" "$CFG"; echo "migrated"; exit "${FAKE_MIGRATE_RC:-0}";;\n', 1)
assert MIGRATING != CLIENT


class AliasBase(SetupBase):
    def setUp(self) -> None:
        super().setUp()
        self.bin = self.prefix / "bin"
        self.alias = self.bin / OLD

    def artifact(self, with_old_only: bool = False, body: str = CLIENT) -> Path:
        self.n_artifacts = getattr(self, "n_artifacts", 0) + 1
        d = self.tmp / f"artifact{self.n_artifacts}"
        d.mkdir()
        name = OLD if with_old_only else "ham"
        (d / name).write_text(body)
        (d / name).chmod(0o755)
        (d / "SHA256SUMS").write_text(f"{sha256_of(d / name)}  {name}\n")
        return d

    def install(self, *extra: str, **env: str):
        return self.run_setup("--from", str(self.artifact()), "--yes", *extra, **env)

    def put_old(self, text: str, mode: int = 0o755) -> Path:
        self.bin.mkdir(parents=True, exist_ok=True)
        self.alias.write_text(text)
        self.alias.chmod(mode)
        return self.alias


class MakingTheLink(AliasBase):
    def test_setup_makes_a_relative_link_to_the_client_beside_it(self) -> None:
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.alias.is_symlink(), self.out(p))
        self.assertEqual(os.readlink(self.alias), "ham", "a relative link: the target has no directory")
        self.assertTrue((self.bin / "ham").is_file())
        self.assertIn("earlier command name", p.stdout)

    def test_a_dry_run_makes_no_link_and_says_what_it_would_do(self) -> None:
        p = self.run_setup("--from", str(self.artifact()), "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(os.path.lexists(self.alias))
        self.assertIn(f"would link {self.alias} to ham", p.stdout)

    def test_a_second_run_leaves_the_link_alone(self) -> None:
        self.assertEqual(self.install().returncode, 0)
        before = os.readlink(self.alias)
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(os.readlink(self.alias), before)
        self.assertIn("already a link", p.stdout)

    def test_the_client_that_was_already_there_gets_no_link_when_it_is_not_in_the_install_folder(self) -> None:
        # another way to install the client (a client on the PATH, outside the prefix) gets no link
        other = self.tmp / "elsewhere"
        other.mkdir()
        self.write_stub("ham", CLIENT, other)
        p = self.run_setup("--yes", PATH=f"{other}:{self.stub}:{self.sysbin}")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(os.path.lexists(self.alias), self.out(p))


class ReplacingAnExistingFile(AliasBase):
    def test_a_link_to_the_client_stays(self) -> None:
        self.bin.mkdir(parents=True)
        os.symlink("ham", self.alias)
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(os.readlink(self.alias), "ham")

    def test_an_earlier_client_that_holds_the_header_name_is_replaced_without_being_run(self) -> None:
        marker = self.tmp / "ran"
        self.put_old(f"#!/bin/sh\n# {OLD_HEADER}\ntouch {marker}\n")
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.alias.is_symlink(), self.out(p))
        self.assertEqual(os.readlink(self.alias), "ham")
        self.assertFalse(marker.exists(), "the earlier file must be recognised without running it")
        self.assertIn("replaced", p.stdout)

    def receipt_with_hash_of_the_old_file(self, folder: str) -> None:
        self.put_old("#!/bin/sh\necho an earlier client without the header text\n")
        receipt = self.home / ".config" / folder / "setup" / "111122223333" / "us-east-1" / "download" / "verified"
        receipt.mkdir(parents=True, exist_ok=True)
        (receipt / "SHA256SUMS").write_text(f"{sha256_of(self.alias)}  linux-amd64/{OLD}\n")

    def test_a_file_whose_hash_is_in_a_receipt_of_this_folder_is_replaced(self) -> None:
        self.receipt_with_hash_of_the_old_file("ham")
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.alias.is_symlink(), self.out(p))

    def test_a_file_whose_hash_is_in_a_receipt_of_the_earlier_folder_is_replaced(self) -> None:
        self.receipt_with_hash_of_the_old_file(OLD)
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.alias.is_symlink(), self.out(p))

    def test_any_other_file_is_kept_with_a_warning(self) -> None:
        self.put_old("#!/bin/sh\necho some other program\n")
        before = tree_state(self.bin)
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertFalse(self.alias.is_symlink())
        self.assertEqual(self.alias.read_text(), "#!/bin/sh\necho some other program\n")
        self.assertIn("warn", p.stdout)
        self.assertIn(str(self.alias), p.stdout)
        self.assertIn("left it as it is", p.stdout)
        self.assertTrue((self.bin / "ham").is_file(), "the client is installed all the same")
        self.assertNotEqual(before, tree_state(self.bin))

    def test_a_link_to_some_other_program_is_kept(self) -> None:
        self.bin.mkdir(parents=True)
        other = self.tmp / "other-tool"
        other.write_text("#!/bin/sh\necho other\n")
        other.chmod(0o755)
        os.symlink(str(other), self.alias)
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertEqual(os.readlink(self.alias), str(other))
        self.assertIn("left it as it is", p.stdout)

    def test_a_directory_of_that_name_is_kept(self) -> None:
        self.bin.mkdir(parents=True)
        self.alias.mkdir()
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue(self.alias.is_dir() and not self.alias.is_symlink())


class TheClientOnThePath(AliasBase):
    def test_a_program_of_the_name_without_the_header_is_not_the_client(self) -> None:
        foreign = "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"${STUB_LOG:?}/ham.log\"\necho 'ham radio logbook'\n"
        self.write_stub("ham", foreign)
        before = (self.stub / "ham").read_text()
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("there is no ham binary to install", p.stderr)
        self.assertIn("does not report a backend version", p.stderr)
        self.assertEqual(self.log_lines(), [], "the foreign program must not be run")
        self.assertEqual((self.stub / "ham").read_text(), before)

    def test_a_program_of_the_name_with_the_header_is_the_client(self) -> None:
        self.write_stub("ham", CLIENT)
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("models", " ".join(self.log_lines()))


class FromAFolderWithOnlyTheEarlierName(AliasBase):
    def test_exit_1_before_anything_is_installed(self) -> None:
        art = self.artifact(with_old_only=True)
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn("earlier name of the client", p.stderr)
        self.assertIn(OLD, p.stderr)
        self.assertEqual(before, tree_state(self.home), "nothing may change")
        self.assertFalse((self.bin / "ham").exists())
        self.assertFalse(os.path.lexists(self.alias))
        self.assertEqual(self.log_lines(), [])


class FromAClientThatIsNotTheCurrentOne(AliasBase):
    """`--from` takes a file or a folder; the checksum proves the file is the one the person named, and the header text proves it is the client of this system."""

    EARLIER = f"#!/bin/sh\n# {OLD_HEADER}\necho an earlier client\n"

    def assert_refused(self, p, before) -> None:
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn(HEADER, p.stderr, "the sentence names the header the file lacks")
        self.assertIn("Nothing is installed", p.stderr)
        self.assertEqual(before, tree_state(self.home), "nothing may change")
        self.assertFalse((self.bin / "ham").exists())
        self.assertFalse(os.path.lexists(self.alias))
        self.assertEqual(self.log_lines(), [])

    def test_an_earlier_client_given_as_a_file_is_refused_before_anything_is_installed(self) -> None:
        art = self.artifact(with_old_only=True, body=self.EARLIER)
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art / OLD), "--yes")
        self.assert_refused(p, before)
        self.assertIn(str(art / OLD), p.stderr)

    def test_an_earlier_client_under_any_name_with_its_checksum_given_by_hand_is_refused(self) -> None:
        f = self.tmp / "old-client"
        f.write_text(self.EARLIER)
        f.chmod(0o755)
        before = tree_state(self.home)
        p = self.run_setup("--from", str(f), "--sha256", sha256_of(f), "--yes")
        self.assert_refused(p, before)

    def test_an_earlier_client_in_a_folder_under_the_new_name_is_refused_too(self) -> None:
        art = self.artifact(body=self.EARLIER)
        before = tree_state(self.home)
        p = self.run_setup("--from", str(art), "--yes")
        self.assert_refused(p, before)

    def test_a_dry_run_refuses_it_as_well(self) -> None:
        art = self.artifact(with_old_only=True, body=self.EARLIER)
        p = self.run_setup("--from", str(art / OLD), "--dry-run")
        self.assertEqual(p.returncode, 1, self.out(p))
        self.assertIn(HEADER, p.stderr)

    def test_the_current_client_given_as_a_file_is_installed(self) -> None:
        art = self.artifact()
        p = self.run_setup("--from", str(art / "ham"), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertTrue((self.bin / "ham").is_file())


class MovingTheSettings(AliasBase):
    def setUp(self) -> None:
        super().setUp()
        self.old_dir = self.home / ".config" / OLD
        self.new_dir = self.home / ".config" / "ham"

    def settings_in_old(self) -> None:
        self.old_dir.mkdir(parents=True)
        (self.old_dir / "config.toml").write_text('region = "us-east-1"\n')

    def test_migrate_runs_in_the_config_step_when_only_the_earlier_folder_holds_settings(self) -> None:
        self.settings_in_old()
        art = self.artifact(body=MIGRATING)
        p = self.run_setup("--from", str(art), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("config migrate", self.log_lines())
        self.assertIn("earlier folder", p.stdout)
        order = self.log_lines()
        self.assertLess(order.index("config migrate"), max(i for i, l in enumerate(order) if l.startswith("rules init")), "before anything is written")

    def test_a_dry_run_says_it_would_migrate(self) -> None:
        self.settings_in_old()
        p = self.run_setup("--from", str(self.artifact()), "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("would run: ham config migrate", p.stdout)
        self.assertEqual(self.log_lines(), [])

    def test_no_migration_when_the_new_folder_holds_settings(self) -> None:
        self.settings_in_old()
        self.new_dir.mkdir(parents=True)
        (self.new_dir / "config.toml").write_text('region = "us-west-2"\n')
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("config migrate", self.log_lines())

    def test_an_empty_new_folder_does_not_count_as_settings(self) -> None:
        self.settings_in_old()
        self.new_dir.mkdir(parents=True)
        p = self.run_setup("--from", str(self.artifact(body=MIGRATING)), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("config migrate", self.log_lines())

    def test_no_migration_without_an_earlier_folder(self) -> None:
        p = self.install()
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertNotIn("config migrate", self.log_lines())

    def test_a_failed_migration_stops_the_setup(self) -> None:
        self.settings_in_old()
        p = self.run_setup("--from", str(self.artifact(body=MIGRATING)), "--yes", FAKE_MIGRATE_RC="2")
        self.assertNotEqual(p.returncode, 0, self.out(p))
        self.assertIn("config migrate failed", p.stderr)
        self.assertIn("still in", p.stderr)


    def migrated(self, p) -> bool:
        self.assertEqual(p.returncode, 0, self.out(p))
        return "config migrate" in self.log_lines()

    def empty_folder_in_the_new_place(self, sub: str) -> None:
        self.settings_in_old()
        (self.new_dir / sub).mkdir(parents=True)
        p = self.run_setup("--from", str(self.artifact(body=MIGRATING)), "--yes")
        self.assertTrue(self.migrated(p), "an empty " + sub + " folder holds nothing of the person's")

    def test_an_empty_engines_folder_of_the_new_place_does_not_count_as_settings(self) -> None:
        self.empty_folder_in_the_new_place("engines")

    def test_an_empty_rules_folder_of_the_new_place_does_not_count_as_settings(self) -> None:
        self.empty_folder_in_the_new_place("rules")

    def test_an_empty_skills_folder_of_the_new_place_does_not_count_as_settings(self) -> None:
        self.empty_folder_in_the_new_place("skills")

    def test_a_folder_that_holds_only_a_file_browsers_file_does_not_count_either(self) -> None:
        self.settings_in_old()
        (self.new_dir / "rules").mkdir(parents=True)
        (self.new_dir / "rules" / ".DS_Store").write_text("")
        p = self.run_setup("--from", str(self.artifact(body=MIGRATING)), "--yes")
        self.assertTrue(self.migrated(p))

    def folder_with_a_file_in_the_new_place(self, sub: str) -> None:
        self.settings_in_old()
        (self.new_dir / sub).mkdir(parents=True)
        (self.new_dir / sub / "one.md").write_text("x\n")
        p = self.install()
        self.assertFalse(self.migrated(p), sub + " holds a file of the person's")

    def test_an_engines_folder_with_a_file_counts_as_settings(self) -> None:
        self.folder_with_a_file_in_the_new_place("engines")

    def test_a_rules_folder_with_a_file_counts_as_settings(self) -> None:
        self.folder_with_a_file_in_the_new_place("rules")

    def test_a_skills_folder_with_a_file_counts_as_settings(self) -> None:
        self.folder_with_a_file_in_the_new_place("skills")

    def test_a_copy_that_stopped_half_way_is_taken_up_again(self) -> None:
        self.settings_in_old()
        self.new_dir.mkdir(parents=True)
        (self.new_dir / "config.toml").write_text('region = "us-west-2"\n')
        (self.new_dir / ".ham-migrating").write_text("")
        p = self.run_setup("--from", str(self.artifact(body=MIGRATING)), "--yes")
        self.assertTrue(self.migrated(p), "the client does not count this folder while the marker is there, so the migration is still due")

    def test_a_dry_run_with_the_marker_says_it_would_migrate(self) -> None:
        self.settings_in_old()
        self.new_dir.mkdir(parents=True)
        (self.new_dir / "config.toml").write_text('region = "us-west-2"\n')
        (self.new_dir / ".ham-migrating").write_text("")
        p = self.run_setup("--from", str(self.artifact()), "--dry-run")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertIn("would run: ham config migrate", p.stdout)

    def test_the_marker_without_settings_in_the_earlier_folder_changes_nothing(self) -> None:
        self.new_dir.mkdir(parents=True)
        (self.new_dir / "config.toml").write_text('region = "us-west-2"\n')
        (self.new_dir / ".ham-migrating").write_text("")
        p = self.install()
        self.assertFalse(self.migrated(p), "there is nothing to copy from")


class TheMenuWithoutAClient(AliasBase):
    """With no client to install the setup lists the ways to get one; it says where two of them stop, so that nobody on an account with a stage under the previous names is sent to them."""

    def test_the_menu_says_where_full_and_update_stop(self) -> None:
        p = self.run_setup("--yes")
        self.assertEqual(p.returncode, 1, self.out(p))
        flat = " ".join(p.stderr.split())
        self.assertIn("(on an account whose stage was deployed under the previous names, --full and --update stop with the steps to take; there use --from or --build)", flat)
        self.assertLess(flat.index("--update "), flat.index("(on an account whose stage"), "the sentence follows the two options it is about")
        self.assertLess(flat.index("(on an account whose stage"), flat.index("--from DIR"), "and comes before the options that work there")


class InstalledLine(AliasBase):
    """The `installed:` line shows the version of the client. When only the earlier folder holds settings the client also prints a notice about it, and that notice is not the version."""

    NOTICING = CLIENT.replace('echo "ham v9.99"', 'echo "Your settings are read from the earlier folder. Copy them with: ham config migrate" >&2; echo "ham v9.99"', 1)

    def test_the_line_shows_the_version_and_not_the_notice_that_comes_first(self) -> None:
        self.assertNotEqual(self.NOTICING, CLIENT)
        p = self.run_setup("--from", str(self.artifact(body=self.NOTICING)), "--yes")
        self.assertEqual(p.returncode, 0, self.out(p))
        self.assertRegex(p.stdout, r"(?m)^\s*ok\s+installed: ham v9\.99$")
        self.assertNotIn("installed: Your settings", p.stdout)


class DefaultRepositoryOfAnEarlierStage(AliasBase):
    def test_both_prefixes_are_accepted_for_the_default_repository(self) -> None:
        for repo in ("cc:hamh-e2e-demo", "cc:" + OLD + "-e2e-demo"):
            with self.subTest(repo=repo):
                p = self.run_setup("--from", str(self.artifact()), "--default-repo", repo, "--dry-run")
                self.assertEqual(p.returncode, 0, self.out(p))

    def test_another_prefix_is_refused(self) -> None:
        p = self.run_setup("--default-repo", "cc:other-demo", "--dry-run")
        self.assertEqual(p.returncode, 2, self.out(p))
        self.assertIn("--default-repo", p.stderr)


if __name__ == "__main__":
    unittest.main()
