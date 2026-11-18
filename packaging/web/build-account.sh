#!/usr/bin/env bash
# Build the native FigCraft browser editor for a WebOS account window.
set -euo pipefail

if [[ "$#" != 1 ]]; then
  echo "Usage: bash packaging/web/build-account.sh NEW_OUTPUT_DIRECTORY" >&2
  exit 2
fi
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
output_parent="$(cd "$(dirname -- "$1")" && pwd -P)"
output="$output_parent/$(basename -- "$1")"
case "$output" in
  "$root"|"$root/"*) echo "Choose an output directory outside the source tree." >&2; exit 2 ;;
esac
: "${CARGO_TARGET_DIR:?Set CARGO_TARGET_DIR to the assigned build cache.}"
: "${CRAFT_FONTS_DIR:?Set CRAFT_FONTS_DIR to the pinned craft-fonts checkout.}"
if [[ "$CARGO_TARGET_DIR" != /* || "$CRAFT_FONTS_DIR" != /* ]]; then
  echo "Use absolute paths for the build cache and font input." >&2
  exit 2
fi
fonts_rev="$(<"$root/packaging/web/craft-fonts.rev")"
if [[ "$(git --no-optional-locks -C "$CRAFT_FONTS_DIR" rev-parse HEAD)" != "$fonts_rev" ]] ||
   [[ -n "$(git --no-optional-locks -C "$CRAFT_FONTS_DIR" status --porcelain --untracked-files=all)" ]]; then
  echo "FigCraft requires a clean craft-fonts checkout at $fonts_rev." >&2
  exit 2
fi

umask 077
mkdir -- "$output"
export DIST="$output" CRAFT_FONTS_REQUIRED=1 CARGO_NET_OFFLINE=true
# shellcheck source=../env.sh
. "$root/packaging/env.sh"
(
  cd "$ROOT/apps/figcraft-web"
  trunk build --release --locked --offline --dist "$output"
)

test -f "$output/index.html"
wasm_files=("$output"/*.wasm)
test "${#wasm_files[@]}" -eq 1
test "$(wc -c < "${wasm_files[0]}")" -le 26214400
copy_docs "$output"
cp "$ROOT/ATTRIBUTION.md" "$output/"
printf 'FigCraft account editor built at %s\n' "$output"
