"""The engine source files of engine/sources/ and their reader.

A source file declares where an engine or plugin comes from: kind, URL, pin, content hashes, build recipe id, SPDX licence id. These cases hold the
strict TOML subset the reader accepts, the validation of each kind (a pin is a full commit or a SHA-256, never a branch or "head"), the in-place
rewrite `engine-fetch update` uses, and the source files of the repository. No network.
"""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts" / "lib"))
import engine_source as es  # noqa: E402

C40 = "1a5c3f84e0933268a9dba14a55a6383a6578414d"
T40 = "7418804b1460929f3ff701bcccc8184fb69c8e26"
B40 = "ef046a4a73036a191a5ba237aca3108880bec9a8"
H64 = "8fa09f470ff4b8e1b86472213d0b84abaf07b09e4c6f2da6d914dbacce09dd30"

GIT_TAG = f'''# comment
schema = "1"
name = "cline"
kind = "git-tag"
license = "Apache-2.0"
recipe = "cline-fork"

[source]
url = "https://github.com/cline/cline.git"
tag = "cli-v3.0.71"   # the release
tag_object = "{T40}"
commit = "{C40}"
patches_lock = "engine/cline/lock.toml"
'''

PATH_BUNDLE = f'''schema = "1"
name = "goal-plugin"
kind = "git-path-bundle"
license = "Apache-2.0"
recipe = "esbuild-bundle"

[source]
url = "https://github.com/cline/plugins.git"
commit = "96bde661f630ec23c1ce0cd86a2361a9959ef65a"

[[files]]
path = "plugins/goal/index.ts"
blob = "{B40}"
sha256 = "{H64}"
as = "goal-index.ts"

[[files]]
path = "LICENSE"
blob = "{B40}"
sha256 = "{H64}"
as = "LICENSE"

[[inputs]]
from = "cline"
path = "sdk/packages/shared/src/tools/create.ts"
blob = "{B40}"
sha256 = "{H64}"
as = "cline/shared/src/tools/create.ts"
'''

RELEASE = f'''schema = "1"
name = "esbuild"
kind = "release-asset"
license = "MIT"
recipe = "none"

[assets.linux-x64]
url = "https://registry.npmjs.org/@esbuild/linux-x64/-/linux-x64-0.25.12.tgz"
sha256 = "{H64}"
member = "package/bin/esbuild"
member_sha256 = "{H64}"

[assets.linux-arm64]
url = "https://registry.npmjs.org/@esbuild/linux-arm64/-/linux-arm64-0.25.12.tgz"
sha256 = "{H64}"
member = "package/bin/esbuild"
member_sha256 = "{H64}"
'''


def problems(text: str, stem: str) -> list:
    return es.validate(es.parse(text), stem)


class ParseCase(unittest.TestCase):
    def test_the_subset_parses_tables_arrays_of_tables_and_values(self) -> None:
        d = es.parse(PATH_BUNDLE)
        self.assertEqual(d["kind"], "git-path-bundle")
        self.assertEqual(d["source"]["commit"], "96bde661f630ec23c1ce0cd86a2361a9959ef65a")
        self.assertEqual([f["as"] for f in d["files"]], ["goal-index.ts", "LICENSE"])
        self.assertEqual(d["inputs"][0]["from"], "cline")
        d2 = es.parse('n = 3\nflag = true\noff = false\nitems = ["a", "b\\"c"]\nneg = -2\n[assets.linux-x64]\nurl = "u"\n')
        self.assertEqual((d2["n"], d2["flag"], d2["off"], d2["items"], d2["neg"]), (3, True, False, ["a", 'b"c'], -2))
        self.assertEqual(d2["assets"]["linux-x64"]["url"], "u")

    def test_a_trailing_comment_is_allowed_and_text_after_a_value_is_not(self) -> None:
        self.assertEqual(es.parse('tag = "x"  # why\n')["tag"], "x")
        for bad in ('tag = "x" y\n', "n = 3 x\n", 'a = ["x" "y"]\n', "a = [1]\n", "t = true x\n"):
            with self.assertRaises(es.SourceError, msg=bad):
                es.parse(bad)

    def test_a_repeated_key_a_repeated_table_and_unknown_syntax_are_errors(self) -> None:
        for bad in ('a = "1"\na = "2"\n', "[t]\n[t]\n", "a: 1\n", "a = 1.5\n", "a = 'x'\n", 'a = """x"""\n', "[t.u.v\n", 'a = "x\\n"\n'):
            with self.assertRaises(es.SourceError, msg=bad):
                es.parse(bad)

    def test_the_real_toml_reader_agrees_with_the_subset_on_the_repository_files(self) -> None:
        try:
            import tomllib  # type: ignore[import-not-found]
        except ImportError:
            self.skipTest("tomllib needs Python 3.11")
        for p in (ROOT / "engine" / "cline" / "lock.toml", ROOT / "engine" / "lock.toml"):
            self.assertEqual(tomllib.loads(p.read_text()), es.parse(p.read_text()), p.name)


class ValidateCase(unittest.TestCase):
    def test_the_three_kinds_validate(self) -> None:
        self.assertEqual(problems(GIT_TAG, "cline"), [])
        self.assertEqual(problems(PATH_BUNDLE, "goal-plugin"), [])
        self.assertEqual(problems(RELEASE, "esbuild"), [])

    def test_the_name_must_be_the_file_name(self) -> None:
        self.assertTrue(any("name must be the file name" in p for p in problems(GIT_TAG, "other")))

    def test_a_pin_is_a_full_commit_never_a_branch_or_head(self) -> None:
        for bad in ("head", "main", "1a5c3f8", C40.upper(), C40[:39]):
            text = GIT_TAG.replace(C40, bad)
            self.assertTrue(any("commit must be a full 40-hex commit" in p for p in problems(text, "cline")), bad)

    def test_a_hash_is_64_hex_and_a_blob_is_40_hex(self) -> None:
        self.assertTrue(any("sha256 must be a SHA-256" in p for p in problems(PATH_BUNDLE.replace(H64, H64[:-1], 1), "goal-plugin")))
        self.assertTrue(any("blob must be a 40-hex git blob id" in p for p in problems(PATH_BUNDLE.replace(B40, "abc", 1), "goal-plugin")))
        self.assertTrue(any("sha256 must be a SHA-256" in p for p in problems(RELEASE.replace(H64, "zz", 1), "esbuild")))

    def test_a_url_is_https(self) -> None:
        self.assertTrue(any("url must be an https URL" in p for p in problems(GIT_TAG.replace("https://github.com", "http://github.com"), "cline")))
        self.assertEqual(es.validate(es.parse(GIT_TAG.replace("https://github.com/cline/cline.git", "/srv/repo")), "cline", allow_local_url=True), [])

    def test_kind_recipe_licence_and_schema(self) -> None:
        self.assertTrue(any("kind must be one of" in p for p in problems(GIT_TAG.replace('"git-tag"', '"svn"'), "cline")))
        self.assertTrue(any("recipe must be one of" in p for p in problems(GIT_TAG.replace('"cline-fork"', '"make"'), "cline")))
        self.assertTrue(any("license must be an SPDX id" in p for p in problems(GIT_TAG.replace('"Apache-2.0"', '"the Apache one"'), "cline")))
        self.assertTrue(any("schema" in p for p in problems(GIT_TAG.replace('schema = "1"', 'schema = "2"'), "cline")))

    def test_unknown_keys_are_refused(self) -> None:
        self.assertTrue(any("unknown key branch" in p for p in problems(GIT_TAG.replace("[source]\n", '[source]\nbranch = "main"\n'), "cline")))
        self.assertTrue(any("unknown key extra" in p for p in problems(RELEASE + 'extra = "1"\n', "esbuild")))

    def test_a_path_bundle_needs_files_and_unique_layout_paths_inside_the_layout(self) -> None:
        no_files = PATH_BUNDLE.split("[[files]]")[0]
        self.assertTrue(any("at least one [[files]]" in p for p in problems(no_files, "goal-plugin")))
        twice = PATH_BUNDLE.replace('as = "LICENSE"', 'as = "goal-index.ts"')
        self.assertTrue(any("used twice" in p for p in problems(twice, "goal-plugin")))
        out = PATH_BUNDLE.replace('as = "LICENSE"', 'as = "../LICENSE"')
        self.assertTrue(any("as must be" in p or "stay inside" in p for p in problems(out, "goal-plugin")))

    def test_an_asset_member_needs_its_hash_and_a_platform_must_be_known(self) -> None:
        self.assertTrue(any("member_sha256 is missing" in p for p in problems(RELEASE.replace('member_sha256 = "%s"\n' % H64, "", 1), "esbuild")))
        self.assertTrue(any("platform must be one of" in p for p in problems(RELEASE.replace("linux-x64", "plan9"), "esbuild")))
        self.assertTrue(any("without member" in p for p in problems('schema = "1"\nname = "o"\nkind = "release-asset"\nlicense = "MIT"\nrecipe = "none"\n[assets.any]\nurl = "https://x.example/a"\nsha256 = "%s"\nmember_sha256 = "%s"\n' % (H64, H64), "o")))


class RewriteCase(unittest.TestCase):
    def test_a_top_level_and_a_table_value_change_in_place_and_everything_else_stays(self) -> None:
        out = es.rewrite(GIT_TAG, "source", "commit", "f" * 40)
        self.assertEqual(out, GIT_TAG.replace(C40, "f" * 40))
        out2 = es.rewrite(GIT_TAG, "source", "tag", "cli-v3.0.72")
        self.assertIn('tag = "cli-v3.0.72"   # the release', out2)
        self.assertEqual(es.rewrite(GIT_TAG, "", "name", "x"), GIT_TAG.replace('name = "cline"', 'name = "x"'))

    def test_the_index_picks_one_table_of_an_array(self) -> None:
        out = es.rewrite(PATH_BUNDLE, "files", "sha256", "a" * 64, index=1)
        d = es.parse(out)
        self.assertEqual([f["sha256"] for f in d["files"]], [H64, "a" * 64])
        out2 = es.rewrite(PATH_BUNDLE, "inputs", "blob", "b" * 40, index=0)
        self.assertEqual(es.parse(out2)["inputs"][0]["blob"], "b" * 40)

    def test_a_missing_key_is_an_error_and_a_quote_is_escaped(self) -> None:
        with self.assertRaises(es.SourceError):
            es.rewrite(GIT_TAG, "source", "nothing", "x")
        with self.assertRaises(es.SourceError):
            es.rewrite(PATH_BUNDLE, "files", "sha256", "x", index=5)
        out = es.rewrite(GIT_TAG, "source", "tag", 'a"b')
        self.assertEqual(es.parse(out)["source"]["tag"], 'a"b')


class RepositoryCase(unittest.TestCase):
    def test_every_source_file_of_the_repository_is_valid(self) -> None:
        d = ROOT / "engine" / "sources"
        self.assertTrue(d.is_dir(), "engine/sources/ holds the source files")
        found = es.load_all(d)
        self.assertEqual(sorted(found), ["cline", "esbuild", "goal-plugin", "opencode"])

    def test_every_source_declares_a_recipe_the_fetch_knows_and_a_licence(self) -> None:
        for name, src in es.load_all(ROOT / "engine" / "sources").items():
            self.assertIn(src.recipe, es.RECIPES, name)
            self.assertTrue(src.license, name)

    def test_the_command_line_lints_the_directory(self) -> None:
        import subprocess  # nosec B404: the test starts the reader with an argument list, never a shell

        with tempfile.TemporaryDirectory() as d:
            (Path(d) / "cline.toml").write_text(GIT_TAG.replace(C40, "main"))
            out = subprocess.run([sys.executable, str(ROOT / "scripts" / "lib" / "engine_source.py"), d], capture_output=True, text=True)  # nosec B603: argument list, no shell, repository script under test
            self.assertEqual(out.returncode, 1, out.stdout)
            self.assertIn("SOURCE-LINT FAIL", out.stdout)
            (Path(d) / "cline.toml").write_text(GIT_TAG)
            ok = subprocess.run([sys.executable, str(ROOT / "scripts" / "lib" / "engine_source.py"), d], capture_output=True, text=True)  # nosec B603: argument list, no shell, repository script under test
            self.assertEqual(ok.returncode, 0, ok.stdout)
            self.assertIn("SOURCE-LINT PASS 1 source file(s): cline", ok.stdout)


if __name__ == "__main__":
    unittest.main()
