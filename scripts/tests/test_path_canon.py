"""Tests for scripts/lib/path-canon.sh: the canonical form of a path, and the question "is this file below that directory".

scripts/node.sh mounts the checkout into its container at the checkout's own path and adds a second mount only for an account file outside it. It used to
ask "outside?" of the path as written, with a string test. The setup runner writes that file at `$CODEBUILD_SRC_DIR/../hamh-setup-work/account/accounts.json`,
a path that begins with the checkout's path and leaves it by a `..`: the string test said "inside", no mount was added, and the CDK app read a file that was
not in the container. The library resolves a path the way the kernel does (`.` and `..`, every symlink, also the last component), for a path that does not
exist yet as well, so the test and the mount see the same path. The cases here pin the resolver against Python's `os.path.realpath` and the decision against
the exact values of the failed build.
"""

from __future__ import annotations

import itertools
import os
import subprocess  # nosec B404: tests start bash with an argument list, never a shell string built from input
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
LIB = REPO / "scripts" / "lib" / "path-canon.sh"

# The values of the build that failed: the checkout CodeBuild extracted the archive to, and the account file the runner laid beside it.
BUILD_ROOT = "/codebuild/output/src545072180/src"
BUILD_FILE = BUILD_ROOT + "/../hamh-setup-work/account/accounts.json"
BUILD_FILE_CLEAN = "/codebuild/output/src545072180/hamh-setup-work/account/accounts.json"


def bash(body: str, *args: str) -> subprocess.CompletedProcess:
    """Runs `body` in a fresh bash with the library sourced; the arguments are $1, $2, ..."""
    return subprocess.run(["bash", "-c", f'. "{LIB}"\n{body}', "bash", *args], capture_output=True, text=True, timeout=60, stdin=subprocess.DEVNULL)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do


class PathCanonBase(unittest.TestCase):
    def setUp(self) -> None:
        # resolved: on a machine whose temp directory is itself a link (macOS) the expected values are the physical ones
        td = tempfile.TemporaryDirectory(prefix="path-canon-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(td.cleanup)
        self.tmp = Path(td.name).resolve()

    def canon(self, path: str) -> str:
        proc = bash('ham_canon_path "$1"', path)
        self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
        return proc.stdout.rstrip("\n")


class CanonPathCase(PathCanonBase):
    def test_the_library_exists_and_defines_the_two_functions(self) -> None:
        self.assertTrue(LIB.is_file(), f"{LIB} does not exist")
        proc = bash("type -t ham_canon_path ham_path_is_under ham_file_mount_args")
        self.assertEqual(proc.stdout.split(), ["function", "function", "function"], proc.stdout + proc.stderr)

    def test_a_clean_path_is_unchanged(self) -> None:
        (self.tmp / "a" / "b").mkdir(parents=True)
        self.assertEqual(self.canon(f"{self.tmp}/a/b"), f"{self.tmp}/a/b")

    def test_dot_dotdot_double_and_trailing_slashes_are_resolved(self) -> None:
        (self.tmp / "a" / "b").mkdir(parents=True)
        (self.tmp / "c").mkdir()
        self.assertEqual(self.canon(f"{self.tmp}//a/./b/../../c/"), f"{self.tmp}/c")

    def test_dotdot_after_a_link_goes_up_from_the_target_of_the_link(self) -> None:
        # The kernel resolves link/.. as the parent of the link's target, which a string clean-up cannot know.
        (self.tmp / "deep" / "target").mkdir(parents=True)
        (self.tmp / "link").symlink_to(self.tmp / "deep" / "target")
        self.assertEqual(self.canon(f"{self.tmp}/link/../x"), f"{self.tmp}/deep/x")
        self.assertEqual(os.path.realpath(f"{self.tmp}/link/../x"), f"{self.tmp}/deep/x")

    def test_a_link_in_the_last_place_is_followed(self) -> None:
        (self.tmp / "real").mkdir()
        (self.tmp / "real" / "file.json").write_text("{}")
        (self.tmp / "file-link.json").symlink_to(self.tmp / "real" / "file.json")
        self.assertEqual(self.canon(f"{self.tmp}/file-link.json"), f"{self.tmp}/real/file.json")

    def test_relative_links_absolute_links_and_chains(self) -> None:
        (self.tmp / "a" / "b").mkdir(parents=True)
        (self.tmp / "one").symlink_to("a/b")                       # relative
        (self.tmp / "two").symlink_to(self.tmp / "one")            # absolute, to a link
        (self.tmp / "a" / "up").symlink_to("../a/b/../b")          # relative with its own ..
        self.assertEqual(self.canon(f"{self.tmp}/two/c"), f"{self.tmp}/a/b/c")
        self.assertEqual(self.canon(f"{self.tmp}/a/up/d"), f"{self.tmp}/a/b/d")

    def test_a_path_that_does_not_exist_is_resolved_as_far_as_it_exists(self) -> None:
        # The account file of a build is named before it is written, and the build's own paths do not exist on the machine that tests them.
        (self.tmp / "real").mkdir()
        (self.tmp / "lnk").symlink_to(self.tmp / "real")
        self.assertEqual(self.canon(f"{self.tmp}/lnk/missing/dir/../x.json"), f"{self.tmp}/real/missing/x.json")
        self.assertEqual(self.canon(f"{self.tmp}/never/../here"), f"{self.tmp}/here")

    def test_dotdot_at_the_top_stays_at_the_top(self) -> None:
        self.assertEqual(self.canon("/../../.."), "/")
        self.assertEqual(self.canon("/"), "/")
        self.assertEqual(self.canon("/.."), "/")

    def test_names_with_spaces_dashes_colons_and_glob_characters_survive(self) -> None:
        odd = self.tmp / "a b" / "-dash" / "c:d" / "[x]*?"
        odd.mkdir(parents=True)
        self.assertEqual(self.canon(f"{odd}/../[x]*?"), str(odd))

    def test_a_relative_or_empty_path_is_refused_with_no_output(self) -> None:
        for value in ("", "a/b", "./a", "../a", "-v"):
            with self.subTest(value=value):
                proc = bash('ham_canon_path "$1"', value)
                self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
                self.assertEqual(proc.stdout, "")

    def test_a_loop_of_links_is_refused_and_does_not_hang(self) -> None:
        (self.tmp / "a").symlink_to(self.tmp / "b")
        (self.tmp / "b").symlink_to(self.tmp / "a")
        proc = bash('ham_canon_path "$1"', f"{self.tmp}/a/x")
        self.assertEqual(proc.returncode, 1, proc.stdout + proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_the_result_matches_realpath_over_a_tree_of_links_and_missing_names(self) -> None:
        # A differential test: every path made of these pieces, resolved by the library and by Python, must agree.
        (self.tmp / "d1" / "d2" / "d3").mkdir(parents=True)
        (self.tmp / "d1" / "f.json").write_text("{}")
        (self.tmp / "to-d2").symlink_to("d1/d2")
        (self.tmp / "d1" / "back").symlink_to("..")
        (self.tmp / "d1" / "d2" / "abs").symlink_to(self.tmp / "d1")
        (self.tmp / "file-link").symlink_to("d1/f.json")
        (self.tmp / "dangling").symlink_to("nowhere/at/all")
        pieces = ["d1", "d2", "d3", "to-d2", "back", "abs", "file-link", "dangling", "nope", "..", ".", "f.json"]
        paths = [str(self.tmp)]
        for n in (1, 2, 3):
            paths += [str(self.tmp) + "/" + "/".join(c) for c in itertools.product(pieces, repeat=n)]
        # one bash for all of them: a process per path would take minutes
        proc = subprocess.run(["bash", "-c", f'. "{LIB}"\nwhile IFS= read -r p; do ham_canon_path "$p" || echo "REFUSED"; done'], input="\n".join(paths) + "\n",  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool; B607: bare name of a standard tool such as git, bash, grep or uv looked up on PATH, as the scripts under test do
                              capture_output=True, text=True, timeout=300)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        got = proc.stdout.split("\n")[:-1]
        self.assertEqual(len(got), len(paths))
        bad = [(p, g, os.path.realpath(p)) for p, g in zip(paths, got) if g != os.path.realpath(p)]
        self.assertEqual(bad[:5], [], f"{len(bad)} of {len(paths)} paths differ from os.path.realpath")


class PathIsUnderCase(PathCanonBase):
    def under(self, directory: str, path: str) -> int:
        return bash('ham_path_is_under "$1" "$2"', directory, path).returncode

    def test_below_is_0_and_beside_or_equal_is_1(self) -> None:
        (self.tmp / "src" / "sub").mkdir(parents=True)
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src/sub/x.json"), 0)
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src"), 1)
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/elsewhere/x.json"), 1)

    def test_a_directory_whose_name_only_starts_the_same_is_not_below(self) -> None:
        (self.tmp / "src").mkdir()
        (self.tmp / "src-other").mkdir()
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src-other/x.json"), 1)

    def test_a_dotdot_that_leaves_the_directory_is_outside_whatever_the_text_starts_with(self) -> None:
        (self.tmp / "src").mkdir()
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src/../work/accounts.json"), 1)

    def test_a_dotdot_that_stays_inside_is_inside(self) -> None:
        (self.tmp / "src" / "sub").mkdir(parents=True)
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src/sub/../accounts.json"), 0)

    def test_links_decide_by_where_they_lead(self) -> None:
        (self.tmp / "src").mkdir()
        (self.tmp / "away").mkdir()
        (self.tmp / "in-src").symlink_to(self.tmp / "src")
        (self.tmp / "src" / "out").symlink_to(self.tmp / "away")
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/in-src/x.json"), 0, "a path through a link to the directory is inside it")
        self.assertEqual(self.under(f"{self.tmp}/src", f"{self.tmp}/src/out/x.json"), 1, "a path through a link out of the directory is outside it")
        self.assertEqual(self.under(f"{self.tmp}/in-src", f"{self.tmp}/src/x.json"), 0, "the directory itself may be given through a link")

    def test_a_path_that_cannot_be_resolved_is_2(self) -> None:
        self.assertEqual(self.under("/", "relative/path"), 2)
        self.assertEqual(self.under("relative", "/abs"), 2)

    def test_the_root_directory_holds_everything(self) -> None:
        self.assertEqual(self.under("/", "/etc/x"), 0)


class RegressionOfTheFailedBuildCase(PathCanonBase):
    """The values of the build that failed: root /codebuild/output/src545072180/src, file <root>/../hamh-setup-work/account/accounts.json."""

    def test_trigger_present_the_old_string_test_calls_the_file_inside_the_checkout(self) -> None:
        # What scripts/node.sh did: a pattern match on the path as written. It must say "inside", or this regression test proves nothing.
        proc = bash('case "$2" in "$1"/*) echo INSIDE ;; *) echo OUTSIDE ;; esac', BUILD_ROOT, BUILD_FILE)
        self.assertEqual(proc.stdout.strip(), "INSIDE", "TRIGGER ABSENT: the string test no longer matches the build's path")

    def test_the_canonical_path_of_the_file_has_no_dotdot(self) -> None:
        proc = bash('ham_canon_path "$1"', BUILD_FILE)
        self.assertEqual(proc.stdout.strip(), BUILD_FILE_CLEAN)

    def test_the_file_is_outside_the_checkout_and_the_builder_adds_the_mount_of_the_cleaned_path(self) -> None:
        proc = bash('ham_file_mount_args "$1" "$2"; echo "rc=$?"; echo "canon=$ham_file_canon"; for a in "${ham_mount_args[@]}"; do echo "arg=$a"; done', BUILD_ROOT, BUILD_FILE)
        lines = proc.stdout.splitlines()
        self.assertEqual(lines, ["rc=0", f"canon={BUILD_FILE_CLEAN}", "arg=-v", f"arg={BUILD_FILE_CLEAN}:{BUILD_FILE_CLEAN}:ro"], proc.stdout + proc.stderr)

    def test_a_file_the_checkout_mount_shows_needs_no_second_mount(self) -> None:
        proc = bash('ham_file_mount_args "$1" "$2"; echo "rc=$?"; echo "n=${#ham_mount_args[@]}"; echo "canon=$ham_file_canon"', BUILD_ROOT, BUILD_ROOT + "/infra/../accounts.json")
        self.assertEqual(proc.stdout.splitlines(), ["rc=0", "n=0", f"canon={BUILD_ROOT}/accounts.json"], proc.stdout + proc.stderr)

    def test_an_unresolvable_file_is_rc_2_with_no_mount(self) -> None:
        proc = bash('ham_file_mount_args "$1" "$2"; echo "rc=$?"; echo "n=${#ham_mount_args[@]}"', BUILD_ROOT, "accounts.json")
        self.assertEqual(proc.stdout.splitlines(), ["rc=2", "n=0"], proc.stdout + proc.stderr)


if __name__ == "__main__":
    unittest.main()
