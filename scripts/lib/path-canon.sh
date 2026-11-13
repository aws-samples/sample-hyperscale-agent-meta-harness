# Sourced, not executed (bash 3.2 or later; no `readlink -f` and no `realpath`, which macOS lacks): the canonical form of a path and the question "is this
# file below that directory". scripts/node.sh uses it to decide which files its container needs a mount for, and its tests pin it.
#
# Why a library: a script that asks "is this path inside the checkout?" with a string test (`case "$file" in "$root"/*`) gets the wrong answer for a path that
# starts with the checkout's path and leaves it by a `..` (`<checkout>/../work/accounts.json`) and for a path that reaches the checkout through a symlink. The
# setup runner names its work folder `$CODEBUILD_SRC_DIR/../hamh-setup-work`; the account file in it was taken for a file of the checkout, no mount was added, and
# the container could not open it. A path is compared only after it is resolved the way the kernel resolves it.
#
# ham_canon_path <absolute path>
#   Prints the physical path: `.`, `..` and repeated slashes resolved, every symlink followed (also the last component), no trailing slash. A `..` after a symlink
#   goes up from the symlink's target, as in the kernel. The path need not exist: a component that does not exist is kept as written and a `..` removes the
#   component before it, as Python's os.path.realpath does. Returns 1 and prints nothing for a relative or empty path and for a chain of more than 40 links.
#
# ham_path_is_under <directory> <path>
#   Returns 0 when the canonical <path> is below the canonical <directory>, 1 when it is the directory itself or is elsewhere, 2 when either cannot be resolved.
#
# ham_file_mount_args <checkout> <file>
#   For a container that mounts <checkout> at its own path and has to see <file> at its own path as well. Sets ham_file_canon to the canonical path of <file> and
#   the array ham_mount_args to the docker arguments that add the file: empty when the checkout mount already shows it, else (-v <canon>:<canon>:ro).
#   Returns 2, with an empty array, when a path cannot be resolved.
ham_canon_path() {
  local rest=$1 out="" comp link hops=0
  case $rest in
    /*) ;;
    *) return 1 ;;
  esac
  rest=${rest#/}
  while [ -n "$rest" ]; do
    case $rest in
      */*) comp=${rest%%/*}; rest=${rest#*/} ;;
      *) comp=$rest; rest="" ;;
    esac
    case $comp in
      "" | .) ;;
      ..) out=${out%/*} ;;
      *)
        if [ -L "$out/$comp" ]; then
          hops=$((hops + 1))
          [ "$hops" -le 40 ] || return 1
          link=$(readlink "$out/$comp") || return 1
          case $link in
            /*) out=""; rest=${link#/}${rest:+/$rest} ;;
            *) rest=$link${rest:+/$rest} ;;
          esac
        else
          out="$out/$comp"
        fi ;;
    esac
  done
  printf '%s\n' "${out:-/}"
}

ham_path_is_under() {
  local dir path base
  dir=$(ham_canon_path "$1") && path=$(ham_canon_path "$2") || return 2
  base=${dir%/}   # empty for the root directory, so that "/" holds every absolute path; a variable, because some bash versions mis-parse "${dir%/}" inside a case pattern
  case $path in
    "$base"/*) return 0 ;;
  esac
  return 1
}

ham_file_mount_args() {
  local rc=0
  ham_mount_args=()
  ham_file_canon=$(ham_canon_path "$2") || { ham_file_canon=""; return 2; }
  ham_path_is_under "$1" "$ham_file_canon" || rc=$?   # a status of 1 is an answer: `set -e` in the caller must not end the script on it
  case $rc in
    0) return 0 ;;
    1) ham_mount_args=(-v "$ham_file_canon:$ham_file_canon:ro"); return 0 ;;
    *) return 2 ;;
  esac
}
