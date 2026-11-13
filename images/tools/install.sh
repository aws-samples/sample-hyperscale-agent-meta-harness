#!/usr/bin/env bash
# Install the gate tool set at <dest>:
#   install.sh <dest> <python3.12> <uv> <gitleaks>
# Callers: scripts/lib/tools_lock.py gate-tools (a Linux machine, dest $HAMH_TOOLS/gate-tools) and
# images/worker-python/Dockerfile (dest /opt/harness/gate-tools, HARNESS_GATES_TOOLS).
# Layout: bin/{uv,gitleaks,python3,ruff,mypy,bandit,pip-audit,diff-cover}; venv/ holds tools.txt;
# wheels/ holds runtime.txt (pytest, pytest-cov, coverage), which the image installs offline
# into each repository venv; manifest.json records the versions and the requirement-file SHA-256s.
# Every package installs with --require-hashes. Prints "GATE-TOOLS OK <tool> <version>" per tool,
# then GATE-TOOLS PASS (exit 0). A failed step exits 1; usage errors exit 2. The two steps that download (the hash-checked
# install and the wheel download) are run again after a failure (fetch.sh retry_run: 4 attempts, pauses of 5, 15 and 45
# seconds); --require-hashes stays on, so a package that stays wrong still fails.
set -euo pipefail
if [ $# -ne 4 ]; then
  echo "usage: install.sh <dest> <python> <uv> <gitleaks>" >&2
  exit 2
fi
dest=$1 python=$2 uv=$3 gitleaks=$4
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=fetch.sh
. "$here/fetch.sh"
for f in "$python" "$uv" "$gitleaks"; do
  if [ ! -x "$f" ]; then
    echo "install.sh: not an executable file: $f" >&2
    exit 2
  fi
done
py_real=$(readlink -f "$python")

rm -rf "$dest"
mkdir -p "$dest/bin"
install -m 0755 "$uv" "$dest/bin/uv"
install -m 0755 "$gitleaks" "$dest/bin/gitleaks"
ln -s "$py_real" "$dest/bin/python3"

UV_PYTHON_DOWNLOADS=never "$dest/bin/uv" venv --quiet --no-cache --python "$py_real" "$dest/venv"
retry_run "uv pip install" "$dest/bin/uv" pip install --quiet --no-cache --python "$dest/venv/bin/python" --require-hashes -r "$here/tools.txt"
retry_run "pip download" "$dest/venv/bin/python" -m pip download --quiet --disable-pip-version-check --no-cache-dir --no-deps \
  --only-binary=:all: --require-hashes -r "$here/runtime.txt" -d "$dest/wheels"
for b in ruff mypy bandit pip-audit diff-cover; do
  ln -s "../venv/bin/$b" "$dest/bin/$b"
done

"$dest/venv/bin/python" - "$dest" "$here" <<'PY'
import hashlib, json, subprocess, sys
from pathlib import Path
dest, here = Path(sys.argv[1]), Path(sys.argv[2])
cmds = {
    "python3": ["python3", "--version"], "uv": ["uv", "--version"], "gitleaks": ["gitleaks", "version"],
    "ruff": ["ruff", "--version"], "mypy": ["mypy", "--version"], "bandit": ["bandit", "--version"],
    "pip-audit": ["pip-audit", "--version"], "diff-cover": ["diff-cover", "--version"],
}
versions = {}
for name, (exe, *args) in cmds.items():
    out = subprocess.run([str(dest / "bin" / exe), *args], capture_output=True, text=True, check=True)
    line = (out.stdout or out.stderr).strip().splitlines()[0]
    versions[name] = line
    print(f"GATE-TOOLS OK {name} {line}")
wheels = sorted(p.name for p in (dest / "wheels").glob("*.whl"))
manifest = {
    "schemaVersion": 1,
    "toolsTxtSha256": hashlib.sha256((here / "tools.txt").read_bytes()).hexdigest(),
    "runtimeTxtSha256": hashlib.sha256((here / "runtime.txt").read_bytes()).hexdigest(),
    "versions": versions,
    "wheels": wheels,
}
(dest / "manifest.json").write_text(json.dumps(manifest, indent=1) + "\n")
PY
echo "GATE-TOOLS PASS $dest"
