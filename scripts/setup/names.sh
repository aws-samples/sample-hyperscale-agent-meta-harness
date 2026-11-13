#!/usr/bin/env bash
# scripts/setup/names.sh: the one definition of the names of the terminal client and of its platform table.
#
# Source it (`. "$here/names.sh"`) from setup.sh and from every script of scripts/setup/. Run directly it prints the definitions, one per line, for
# tools that are not bash:  HAMH_CLIENT_BIN=ham ... and  PLATFORM <platform> <rust target> <binary kind>.
# A script of this feature runs the client as "$HAMH_CLIENT_BIN", builds paths below the config directory with "$HAMH_CONFIG_DIR_NAME" and never
# writes the literal word; scripts/tests/test_setup_names.py fails a script that does, in a command or in a message. Changing the client's name means changing it here, the names of resources, the Rust crate and binary names, and the documents.
#
#   HAMH_CLIENT_BIN            the command the user types, and the cargo package and binary it is built from (default ham)
#   HAMH_ADMIN_BIN             the operator tool built and installed beside the client, and the cargo package and binary it is built from (default
#                             "$HAMH_CLIENT_BIN"-admin, so renaming the client renames it)
#   HAMH_PRODUCT_NAME          the product's name in text that names the product (default Hyperscale Agent Meta Harness); no message of setup.sh prints it
#   HAMH_CONFIG_DIR_NAME       the directory below $XDG_CONFIG_HOME (default ~/.config) that holds the client's files (default ham)
#   HAMH_CLIENT_PLATFORM_TABLE one row per client platform, "<platform> <rust target> <binary kind>", comments with # and blank lines allowed
#   LEGACY_CLIENT_BIN     the command the client had before the rename to the current name (cah); setup.sh keeps a link of this name to the client (COMPAT-CAH)
#   HAMH_LEGACY_CONFIG_DIR_NAME the settings directory the client had before the rename (cah), read-only; `ham config migrate` copies it (COMPAT-CAH)
#   HAMH_RUNNER_CONTRACT       the number of the contract between the setup files that run on your machine (the runner template, preflight, the reader of the result) and
#                             the files a build runs from the commit it builds (the buildspec, runner-build.sh, runner_state.py, deploy.py). An update with --revision of a
#                             commit whose names.sh has another number, or none, is refused before anything is created: that commit's own setup.sh runs it. A change that
#                             alters that contract raises the number. It is not taken from the environment.
#
# The environment overrides all five (the tests do). Each is checked when the file is sourced: a value the scripts could not use
# as a file name or a cargo argument makes the source fail with the reason on stderr (return 1).
#
# The variables of the earlier spelling (COMPAT-CAH): hamh_env_compat copies the variable of the earlier prefix to the one of the current prefix for the names in
# ENV_COMPAT_NAMES (or the names it is given), with one line on stderr per name, and unsets the earlier one so that nothing below this process sees two spellings of one
# setting. The current name wins when both are set. For the three that choose the account, the account file or the owner (ENV_COMPAT_SELECTORS) two different values are an
# error (return 2): a selector is never ignored and never replaced by a default. HAMH_NO_DEPRECATION_NOTICES=1 silences the line. The helper runs first when this file is sourced,
# so the four name variables below take their earlier spelling too.
#
# Functions (they print one answer on stdout and return 1 with the reason on stderr):
#   ham_client_platforms             the platforms of the table, one per line, in table order
#   ham_client_target <platform>     the Rust target the platform is built for
#   ham_client_kind <platform>       what the built file must be: macho-arm64 (a signed thin arm64 Mach-O), elf-x86_64 or elf-aarch64 (a static ELF)
#   ham_host_platform [os] [arch]    the platform of a machine, from `uname -s` and `uname -m` unless given; refuses a machine with no row
#
# The operator tool is built for the same platforms as the client, in the same cargo run, and checked the same way.
# The table (each row is built by scripts/setup/build-client.sh; the Linux rows are static musl builds, so they start on any distribution):
#   darwin-arm64   aarch64-apple-darwin        Apple Silicon Macs. The zig linker signs the file ad hoc, which Apple Silicon requires.
#                                              macOS 13 is the lowest version the pinned zig (0.16.0) supports (its release notes). setup.sh does not check the version of the Mac;
#                                              client_check.py reports the minimum the file declares (minos) and accepts any.
#   linux-amd64    x86_64-unknown-linux-musl   Linux on x86-64.
#   linux-arm64    aarch64-unknown-linux-musl  Linux on AArch64.
# No Intel Mac row: the x86_64-apple-darwin build works, but the linker does not sign it and the setup supports Apple Silicon only. A platform that
# cannot be built is a row removed from here, with the reason in this comment; the build script never skips a row on its own.
#
# Bash 3.2 (the bash that ships with macOS); no program is started when the file is sourced.

# The suffixes of the variables setup reads or hands on that had the earlier prefix.
# The stage settings (the rows of scripts/lib/stage-settings.tsv) are not in this list: pf_fold_earlier_settings of preflight.sh folds the earlier spelling of every one of them
# in one loop over the registry, and ends the run when both spellings are set and differ.
ENV_COMPAT_NAMES="CLIENT_BIN ADMIN_BIN PRODUCT_NAME CONFIG_DIR_NAME AWS_CLI SETUP_AUTHORITY_HOURS SETUP_BUILD_MINUTES SETUP_BUILD_COMPUTE SETUP_POLL_SECONDS SETUP_POLL_RETRIES SETUP_STACK_LOOKS AWS_PROFILE ACCOUNTS_FILE OWNER REPO_NAMES"
ENV_COMPAT_SELECTORS="AWS_PROFILE ACCOUNTS_FILE OWNER"

# hamh_env_compat [SUFFIX ...]: see the header. Returns 0, or 2 when a selector has two different values (the reason is on stderr).
hamh_env_compat() {
  local old new suffix vold vnew
  # the exported variables of the earlier prefix are found with shell builtins alone (compgen, case): the usual run starts no program, and sourcing needs none
  for old in $(compgen -e); do
    case $old in
      CAH_*) ;;  # COMPAT-CAH
      *) continue ;;
    esac
    suffix=${old#CAH_}  # COMPAT-CAH
    case " ${*:-$ENV_COMPAT_NAMES} " in
      *" $suffix "*) ;;
      *) continue ;;
    esac
    new=HAMH_$suffix
    vold=$(printenv "$old")
    vnew=$(printenv "$new" || true)
    if [ -z "$vnew" ]; then
      export "$new=$vold"
      _hamh_notice "$old is the previous name of $new; both work in this release, and $new takes precedence when both are set. Set $new instead."
    elif [ "$vnew" != "$vold" ]; then
      case " $ENV_COMPAT_SELECTORS " in
        *" $suffix "*)
          printf 'names.sh: %s and %s are both set and their values differ. A variable that chooses the account, the account file or the owner is never ignored and never replaced by a default. Unset one of them. Nothing was changed.\n' "$new" "$old" >&2
          return 2 ;;
      esac
      _hamh_notice "$old is the previous name of $new; both work in this release, and $new takes precedence when both are set. Set $new instead."
    fi
    unset "$old"
  done
  return 0
}

# _hamh_notice <text>: one line on stderr per call, unless HAMH_NO_DEPRECATION_NOTICES=1.
_hamh_notice() {
  [ "${HAMH_NO_DEPRECATION_NOTICES:-}" != 1 ] || return 0
  printf 'names.sh: %s Set HAMH_NO_DEPRECATION_NOTICES=1 to silence this notice.\n' "$1" >&2
}

hamh_env_compat || { return 2 2>/dev/null || exit 2; }

: "${HAMH_CLIENT_BIN:=ham}"
: "${HAMH_ADMIN_BIN:=${HAMH_CLIENT_BIN}-admin}"
: "${HAMH_PRODUCT_NAME:=Hyperscale Agent Meta Harness}"
: "${HAMH_CONFIG_DIR_NAME:=ham}"
: "${LEGACY_CLIENT_BIN:=cah}"  # COMPAT-CAH
: "${HAMH_LEGACY_CONFIG_DIR_NAME:=cah}"  # COMPAT-CAH
HAMH_RUNNER_CONTRACT=1
if [ -z "${HAMH_CLIENT_PLATFORM_TABLE:-}" ]; then
  HAMH_CLIENT_PLATFORM_TABLE='darwin-arm64 aarch64-apple-darwin macho-arm64
linux-amd64 x86_64-unknown-linux-musl elf-x86_64
linux-arm64 aarch64-unknown-linux-musl elf-aarch64'
fi

_ham_names_err() { printf 'names.sh: %s\n' "$*" >&2; }

# _ham_names_check: the values are usable as file names and cargo arguments (return 1 with the reason).
_ham_names_check() {
  case $HAMH_CLIENT_BIN in
    ''|[!a-z]*|*[!a-z0-9-]*) _ham_names_err "HAMH_CLIENT_BIN must be lowercase letters, digits and dashes, starting with a letter (got: $HAMH_CLIENT_BIN)"; return 1 ;;
  esac
  case $HAMH_ADMIN_BIN in
    ''|[!a-z]*|*[!a-z0-9-]*) _ham_names_err "HAMH_ADMIN_BIN must be lowercase letters, digits and dashes, starting with a letter (got: $HAMH_ADMIN_BIN)"; return 1 ;;
  esac
  if [ "$HAMH_ADMIN_BIN" = "$HAMH_CLIENT_BIN" ]; then
    _ham_names_err "HAMH_ADMIN_BIN must differ from HAMH_CLIENT_BIN (both are $HAMH_ADMIN_BIN)"
    return 1
  fi
  case $HAMH_CONFIG_DIR_NAME in
    ''|.*|*[!A-Za-z0-9._-]*) _ham_names_err "HAMH_CONFIG_DIR_NAME must be letters, digits, dot, underscore and dash, not starting with a dot (got: $HAMH_CONFIG_DIR_NAME)"; return 1 ;;
  esac
  case $HAMH_PRODUCT_NAME in
    *'
'*) _ham_names_err "HAMH_PRODUCT_NAME must be one line"; return 1 ;;
  esac
  return 0
}

# _ham_table_rows: print the rows of the table without comments and blank lines, each as "platform target kind"; return 1 on a malformed row.
_ham_table_rows() {
  local line p t k extra out=""
  while IFS= read -r line; do
    case $line in
      ''|'#'*) continue ;;
    esac
    p=""; t=""; k=""; extra=""
    read -r p t k extra <<< "$line"
    if [ -z "$p" ] || [ -z "$t" ] || [ -z "$k" ] || [ -n "$extra" ]; then
      _ham_names_err "malformed platform table row (want: <platform> <rust target> <binary kind>): $line"
      return 1
    fi
    case $p in
      -*|*[!a-z0-9-]*) _ham_names_err "malformed platform table row (platform: lowercase letters, digits and dashes): $line"; return 1 ;;
    esac
    case $t in
      -*|*[!a-z0-9_-]*) _ham_names_err "malformed platform table row (Rust target: lowercase letters, digits, underscores and dashes): $line"; return 1 ;;
    esac
    case $k in
      macho-arm64|elf-x86_64|elf-aarch64) ;;
      *) _ham_names_err "malformed platform table row (unknown binary kind $k; want macho-arm64, elf-x86_64 or elf-aarch64): $line"; return 1 ;;
    esac
    out="$out$p $t $k
"
  done <<< "$HAMH_CLIENT_PLATFORM_TABLE"
  printf '%s' "$out"
}

ham_client_platforms() {
  local rows p t k
  rows=$(_ham_table_rows) || return 1
  while read -r p t k; do
    [ -n "$p" ] && printf '%s\n' "$p"
  done <<< "$rows"
  return 0
}

# _ham_lookup <platform> <field number: 2 target, 3 kind>
_ham_lookup() {
  local want=$1 field=$2 rows p t k known=""
  rows=$(_ham_table_rows) || return 1
  while read -r p t k; do
    [ -n "$p" ] || continue
    known="$known${known:+ }$p"
    if [ "$p" = "$want" ]; then
      if [ "$field" = 2 ]; then printf '%s\n' "$t"; else printf '%s\n' "$k"; fi
      return 0
    fi
  done <<< "$rows"
  _ham_names_err "unknown client platform '$want' (known: $known)"
  return 1
}

ham_client_target() { _ham_lookup "${1:-}" 2; }
ham_client_kind() { _ham_lookup "${1:-}" 3; }

ham_host_platform() {
  local os=${1:-} arch=${2:-} platform
  [ -n "$os" ] || os=$(uname -s)
  [ -n "$arch" ] || arch=$(uname -m)
  case "$os/$arch" in
    Darwin/arm64) platform=darwin-arm64 ;;
    Darwin/x86_64)
      _ham_names_err "this Mac has an Intel processor. The client is built for Apple Silicon Macs (darwin-arm64) and for Linux only."
      return 1 ;;
    Linux/x86_64|Linux/amd64) platform=linux-amd64 ;;
    Linux/aarch64|Linux/arm64) platform=linux-arm64 ;;
    *)
      _ham_names_err "no client is built for $os on $arch (built: darwin-arm64, linux-amd64, linux-arm64)"
      return 1 ;;
  esac
  ham_client_target "$platform" >/dev/null || return 1
  printf '%s\n' "$platform"
}

_ham_names_check || { return 1 2>/dev/null || exit 1; }
export HAMH_CLIENT_BIN HAMH_ADMIN_BIN HAMH_PRODUCT_NAME HAMH_CONFIG_DIR_NAME HAMH_CLIENT_PLATFORM_TABLE HAMH_LEGACY_CONFIG_DIR_NAME

if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  printf 'HAMH_CLIENT_BIN=%s\nHAMH_ADMIN_BIN=%s\nHAMH_PRODUCT_NAME=%s\nHAMH_CONFIG_DIR_NAME=%s\n' "$HAMH_CLIENT_BIN" "$HAMH_ADMIN_BIN" "$HAMH_PRODUCT_NAME" "$HAMH_CONFIG_DIR_NAME"
  rows=$(_ham_table_rows) || exit 1
  while read -r p t k; do
    [ -n "$p" ] && printf 'PLATFORM %s %s %s\n' "$p" "$t" "$k"
  done <<< "$rows"
fi
