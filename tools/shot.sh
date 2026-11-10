#!/bin/bash
# Drive the real UI with scripted input and write screenshots.
#
#   tools/shot.sh <repo|none> <out.png> "<script>"
#
#   tools/shot.sh /path/to/repo shots/a.png "click:560,178;shot:menu.png;key:Escape"
#
# The script language is documented at the top of src/shot.rs. Intermediate
# `shot:` files land next to <out.png>. Settings are kept in a `cfg` folder
# beside the output so the real user configuration is never touched, git's
# included: GIT_CONFIG_GLOBAL points at cfg/gitconfig.
#
# Environment: SIZE=1500x860 (window size in points), BIN=path/to/gitkrill.

set -u
here="$(cd "$(dirname "$0")/.." && pwd)"
repo="$1"
out="$2"
script="${3:-}"

mkdir -p "$(dirname "$out")"
outdir="$(cd "$(dirname "$out")" && pwd)"
out="$outdir/$(basename "$out")"

args=()
[ "$repo" != "none" ] && args+=("$repo")

# The debug build, whatever the platform calls it.
bin="${BIN:-$here/target/debug/gitkrill}"
[ -x "$bin" ] || bin="$bin.exe"

cfg="${GITKRILL_CONFIG_DIR:-$outdir/cfg}"
mkdir -p "$cfg"
# Git settings changed from the preferences page (`git config --global`)
# land in the run's own file, never in the real ~/.gitconfig. It starts out
# with an author so commits work in repositories that set none.
[ -f "$cfg/gitconfig" ] || printf '[user]\n\tname = GitKrill Shot\n\temail = shot@example.invalid\n' > "$cfg/gitconfig"

GITKRILL_CONFIG_DIR="$cfg" \
GIT_CONFIG_GLOBAL="${GIT_CONFIG_GLOBAL:-$cfg/gitconfig}" \
GITKRILL_SHOT="$out" \
GITKRILL_SHOT_SIZE="${SIZE:-1500x860}" \
GITKRILL_SHOT_SCRIPT="$script" \
    timeout 120 "$bin" "${args[@]}"
status=$?
[ $status -ne 0 ] && echo "gitkrill exited with $status" >&2
exit $status
