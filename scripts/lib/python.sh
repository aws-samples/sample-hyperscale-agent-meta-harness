#!/usr/bin/env bash
# Sourced by the script wrappers: sets $py to a Python 3.8+ interpreter. Order: $HAMH_PYTHON, the managed CPython 3.12 of
# $UV_PYTHON_INSTALL_DIR, python3.12, python3.11, ~/.local/bin/python3.11, python3.
ham_pick_python() {
  local min="${1:-8}" c m
  local managed=()
  if [ -n "${UV_PYTHON_INSTALL_DIR:-}" ]; then
    for m in "$UV_PYTHON_INSTALL_DIR"/cpython-3.12*/bin/python3.12; do
      [ -x "$m" ] && managed+=("$m")
    done
  fi
  for c in "${HAMH_PYTHON:-}" ${managed[@]+"${managed[@]}"} python3.12 python3.11 "$HOME/.local/bin/python3.11" python3; do
    [ -n "$c" ] || continue
    if command -v "$c" >/dev/null 2>&1 && "$c" -c "import sys; sys.exit(0 if sys.version_info >= (3, $min) else 1)" 2>/dev/null; then
      printf '%s' "$c"
      return 0
    fi
  done
  echo "no Python 3.$min+ interpreter found (set HAMH_PYTHON)" >&2
  return 1
}
py="$(ham_pick_python)"
