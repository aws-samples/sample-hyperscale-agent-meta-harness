"""The NTIER-events prompt must ask for a Go version the module store's modules allow.

The harness resolves Go modules offline from its module store, which holds only the versions of
`engine/rules/deps/go-approved-modules.txt`, and its lock step's `go mod tidy` raised the `go` line of `go.mod` to the highest
`go` directive in the module graph. The only approved pgx (v5.11.0) says `go 1.25.0` and the golang.org/x pins say `go
1.26.0`, so tidy rewrote `go 1.24`; foundation's merged test asserted `go 1.24`, and the task that added pgx could change
neither file. Go orders `1.26` below `1.26.0`, so asking for `Go 1.26` would be rewritten too. The gates build with
GOTOOLCHAIN=local, so the version must also not exceed the gate toolchain the approved list names (`# go:` header). The clean
room's tier images (`fixtures/ntier/events/cleanroom.json`) must run the same version: the official golang images set
GOTOOLCHAIN=local too.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess  # nosec B404: tests start the scripts under test and git with argument lists, never a shell
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROMPT = ROOT / "fixtures/ntier/events/prompt.md"
CLEANROOM = ROOT / "fixtures/ntier/events/cleanroom.json"
APPROVED = ROOT / "engine/rules/deps/go-approved-modules.txt"

# The `go` directive of each module's go.mod at the approved version, read on 2026-10-07 with the gate toolchain
# (go1.27.1): `go mod download -json <module>@<version>` with GOPROXY=direct, then the `go` line of its GoMod file.
# When the approved list moves to a version this table does not name, `test_the_recorded_directives_cover_the_approved_versions`
# fails and the table is re-measured the same way.
RECORDED = {
    ("github.com/jackc/pgx/v5", "v5.11.0"): "1.25.0",
    ("github.com/lib/pq", "v1.12.3"): "1.21",
    ("github.com/rabbitmq/amqp091-go", "v1.15.0"): "1.20",
    ("golang.org/x/sync", "v0.23.0"): "1.26.0",
    ("golang.org/x/sys", "v0.48.0"): "1.26.0",
    ("golang.org/x/text", "v0.42.0"): "1.26.0",
    ("golang.org/x/time", "v0.16.0"): "1.26.0",
}
# What the events stack (PostgreSQL and RabbitMQ) imports from the store: its PostgreSQL drivers and its RabbitMQ client.
# Every golang.org/x root is added: the list's header calls them the pins the harness's lock step raises.
STACK = ("github.com/jackc/pgx/v5", "github.com/lib/pq", "github.com/rabbitmq/amqp091-go")


def go_version(v: str) -> tuple:
    """A Go version as Go orders it (go/version): a language version `1.26` sorts below its first release `1.26.0`."""
    m = re.fullmatch(r"(?:go)?(\d+)\.(\d+)(?:\.(\d+))?", v.strip())
    if not m:
        raise ValueError(v)
    return int(m.group(1)), int(m.group(2)), int(m.group(3)) if m.group(3) is not None else -1


def prompt() -> str:
    return " ".join(PROMPT.read_text().split())


def approved() -> dict:
    out = {}
    for line in APPROVED.read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "@" in line:
            module, version = line.rsplit("@", 1)
            out[module] = version
    return out


def gate_toolchain() -> str:
    for line in APPROVED.read_text().splitlines():
        m = re.match(r"#\s*go:\s*go(\d+\.\d+(?:\.\d+)?)\b", line)
        if m:
            return m.group(1)
    raise AssertionError("the approved list names no gate toolchain (`# go:` header)")


def needed() -> dict:
    a = approved()
    mods = [m for m in STACK] + sorted(m for m in a if m.startswith("golang.org/x/"))
    return {m: a.get(m) for m in mods}


def asked() -> str:
    versions = set(re.findall(r"\bGo (\d+\.\d+(?:\.\d+)?)\b", prompt()))
    if len(versions) != 1:
        raise AssertionError(f"the prompt must name exactly one Go version, it names {sorted(versions)}")
    return versions.pop()


class GoOrdering(unittest.TestCase):
    def test_a_language_version_sorts_below_its_first_release(self):
        self.assertLess(go_version("1.26"), go_version("1.26.0"))
        self.assertLess(go_version("1.26.0"), go_version("1.27"))
        self.assertLess(go_version("1.27"), go_version("1.27.1"))
        self.assertLess(go_version("1.24"), go_version("1.25.0"))
        self.assertEqual(go_version("go1.27.1"), go_version("1.27.1"))
        with self.assertRaises(ValueError):
            go_version("1.27rc1")


class EventsPrompt(unittest.TestCase):
    def test_the_recorded_directives_cover_the_approved_versions(self):
        for module, version in needed().items():
            self.assertIsNotNone(version, f"{module} is not in the approved list; update STACK")
            self.assertIn((module, version), RECORDED,
                          f"{module}@{version}: re-measure its go directive (go mod download -json, GOPROXY=direct)")

    def test_the_prompt_names_one_go_version_and_the_same_clean_room_image(self):
        go = asked()
        images = set(re.findall(r"`golang:(\d+\.\d+(?:\.\d+)?)`", prompt()))
        self.assertEqual(images, {go}, "the clean room the prompt names runs the Go version it asks for")

    def test_the_go_version_is_at_least_every_go_line_the_stack_needs(self):
        go = asked()
        lines = {m: RECORDED[(m, v)] for m, v in needed().items()}
        top = max(lines.values(), key=go_version)
        self.assertGreaterEqual(
            go_version(go), go_version(top),
            f"the prompt asks for Go {go}, but tidy raises go.mod to go {top} for "
            + ", ".join(sorted(m for m, x in lines.items() if go_version(x) > go_version(go))))

    def test_the_go_version_is_not_above_the_gate_toolchain(self):
        self.assertLessEqual(go_version(asked()), go_version(gate_toolchain()),
                             f"the gates build with go{gate_toolchain()} and GOTOOLCHAIN=local")

    def test_every_clean_room_tier_image_runs_the_go_version_the_prompt_asks_for(self):
        # The official golang images set GOTOOLCHAIN=local, so a tier image older than the module's `go` line cannot build
        # it.
        go = asked()
        tiers = json.loads(CLEANROOM.read_text())["tiers"]
        self.assertEqual(sorted(t["tier"] for t in tiers), ["e2e", "integration", "unit"])
        for t in tiers:
            m = re.fullmatch(r"golang:(\d+\.\d+(?:\.\d+)?)(?:-[a-z0-9]+)?@sha256:[0-9a-f]{64}", t["image"])
            self.assertIsNotNone(m, f"{t['name']}: {t['image']} is not a golang image pinned by digest")
            self.assertEqual(m.group(1), go, f"{t['name']}: {t['image']} does not run the Go {go} the prompt asks for")


# Tool versions the prompt's scripts run with `go run <module>@<version>` that do not build on the gate toolchain (go1.27.1), measured on
# 2026-10-14 with `go run gotest.tools/gotestsum@v1.12.0 --version` (GOTOOLCHAIN=local): golang.org/x/tools v0.19.0, internal/tokeninternal,
# "invalid array length -delta * delta". The clean room runs the same Go with network, so a script that follows the prompt to the letter
# fails its unit tier there. v1.13.0 builds.
BROKEN_TOOLS = {"gotest.tools/gotestsum": {"v1.12.0"}}
TOOL_ROOTS = ROOT / "scripts/gomod-tool-roots.txt"


def tools_named() -> list:
    """(module, version) of every `go run`/`go install` `<module>@<version>` in the prompt and the clean room spec of each sample."""
    found = []
    for text in (prompt(), CLEANROOM.read_text()):
        found += re.findall(r"go (?:run|install) ([\w./-]+)@(v\d+\.\d+\.\d+)", text)
    return sorted(set(found))


class ToolVersions(unittest.TestCase):
    def test_the_prompt_names_a_gotestsum_version_that_builds_on_the_gate_toolchain(self):
        named = tools_named()
        self.assertTrue(named, "the prompt runs gotestsum with go run")
        for module, version in named:
            self.assertNotIn(version, BROKEN_TOOLS.get(module, set()),
                             f"go run {module}@{version} does not build on go{gate_toolchain()}; name a version that does")

    def test_every_tool_the_prompt_runs_is_a_tool_root_of_the_direct_module_store(self):
        roots = {ln.split("#")[0].strip() for ln in TOOL_ROOTS.read_text().splitlines() if ln.split("#")[0].strip()}
        for module, version in tools_named():
            self.assertIn(f"{module}@{version}", roots, "the session can only run a tool the store holds")

    @unittest.skipUnless(os.environ.get("HAMH_FIXTURE_TOOLCHAIN"), "needs network; set HAMH_FIXTURE_TOOLCHAIN=1 (the real toolchain check)")
    def test_the_real_go_toolchain_builds_every_tool_version_the_prompt_names(self):
        go = os.environ.get("HAMH_FIXTURE_GO") or shutil.which("go")
        self.assertTrue(go, "no go: set HAMH_FIXTURE_GO")
        tmp = Path(tempfile.mkdtemp(prefix="tools-", dir=os.environ.get("TMPDIR")))
        env = {**os.environ, "GOTOOLCHAIN": "local", "GOFLAGS": "-mod=mod", "GOPROXY": os.environ.get("HAMH_FIXTURE_GOPROXY", "direct"),
               "GOSUMDB": "off", "GOENV": "off", "GOCACHE": str(tmp / "cache"), "GOMODCACHE": str(tmp / "mod"), "GOPATH": str(tmp / "path")}
        (tmp / "proj").mkdir()
        (tmp / "proj" / "go.mod").write_text("module example.invalid/p\n\ngo 1.27\n")
        for module, version in tools_named():
            r = subprocess.run([go, "run", f"{module}@{version}", "--version"], cwd=tmp / "proj", env=env, capture_output=True, text=True, timeout=900)  # nosec B603: argument list, no shell, program under test that is the interpreter, a repository script or a standard tool
            self.assertEqual(r.returncode, 0, f"go run {module}@{version}: {(r.stderr or r.stdout)[-400:]}")


if __name__ == "__main__":
    unittest.main()
