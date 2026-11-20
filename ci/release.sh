#!/usr/bin/env bash
# Static x86_64-unknown-linux-musl release of the shipped binaries (ADR-0042 section 10).
#   ci/release.sh OUT_DIR [REBUILD_DIR]
# Builds into ./target with the reproducibility settings of section 10, copies the binaries into
# OUT_DIR, writes OUT_DIR/SHA256SUMS over exactly them and fails unless each one is static. With
# REBUILD_DIR, which must not exist, it builds a second time from a clean REBUILD_DIR/target at
# the same time and fails, naming the file, unless both SHA256SUMS are identical. Results are
# GitHub annotations: one notice per binary, one with SHA256SUMS, one with the toolchain, and one
# with the comparison or one per differing file.
set -euo pipefail

TRIPLE=x86_64-unknown-linux-musl
BINS=(ulei ulei-inbox ulei-engine ulei-web ulei-tg)

usage() {
  echo "usage: ci/release.sh OUT_DIR [REBUILD_DIR]" >&2
  exit 2
}

[ "$#" -eq 1 ] || [ "$#" -eq 2 ] || usage
absolute() {
  case "$1" in
    /*) echo "$1" ;;
    *) echo "$PWD/$1" ;;
  esac
}
out=$(absolute "$1")
rebuild=""
if [ "$#" -eq 2 ]; then rebuild=$(absolute "$2"); fi

root=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$root"
cargo_home=$(mkdir -p "${CARGO_HOME:-$HOME/.cargo}" && cd "${CARGO_HOME:-$HOME/.cargo}" && pwd -P)
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct)
export SOURCE_DATE_EPOCH CARGO_INCREMENTAL=0
unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_RUSTFLAGS

# Builds the release into the target directory $1 and stages it with SHA256SUMS in $2.
build() (
  mkdir -p "$1" "$2"
  target_dir=$(cd "$1" && pwd -P)
  out_dir=$(cd "$2" && pwd -P)
  # The last matching remap wins, so the target directory, which may lie in the checkout, is last.
  rustflags="" cflags=""
  for map in "$root=/ulei" "$cargo_home=/cargo" "$target_dir=/target"; do
    case "$map" in
      *[[:space:]]*) echo "release: a build path holds whitespace: $map" >&2; exit 1 ;;
    esac
    rustflags="$rustflags --remap-path-prefix=$map"
    cflags="$cflags -ffile-prefix-map=$map"
  done
  export RUSTFLAGS="${rustflags# }" TARGET_CFLAGS="${cflags# }"
  # Separate builds: one build of the packages unifies their features and changes the binaries.
  cargo_release() { cargo build --release --locked --target "$TRIPLE" --target-dir "$target_dir" "$@"; }
  cargo_release -p ulei-core
  cargo_release -p ulei-engine
  cargo_release -p ulei-engine --features web --bin ulei-web
  cargo_release -p ulei-channels --bin ulei-tg
  for name in "${BINS[@]}"; do
    install -m 0755 "$target_dir/$TRIPLE/release/$name" "$out_dir/$name"
  done
  cd "$out_dir"
  sha256sum "${BINS[@]}" > SHA256SUMS
)

annotate() {
  local level=$1 title=$2 message=$3
  message=${message//'%'/'%25'}
  message=${message//$'\n'/'%0A'}
  echo "::$level title=$title::$message"
}

if [ -n "$rebuild" ]; then
  if [ -e "$rebuild" ]; then
    annotate error reproducibility "the rebuild directory $rebuild exists; it must be clean"
    exit 1
  fi
  mkdir -p "$rebuild"
  build "$rebuild/target" "$rebuild/out" > "$rebuild/build.log" 2>&1 &
  rebuild_pid=$!
fi

build "$root/target" "$out"

status=0
for bin in "${BINS[@]}"; do
  desc=$(file -b "$out/$bin")
  case "$desc" in
    *interpreter* | *"dynamically linked"*)
      annotate error "static $bin" "$desc"; status=1 ;;
    *"static-pie linked"* | *"statically linked"*)
      annotate notice "static $bin" "$desc" ;;
    *)
      annotate error "static $bin" "not a static executable: $desc"; status=1 ;;
  esac
done
annotate notice SHA256SUMS "$(cat "$out/SHA256SUMS")"
annotate notice toolchain \
  "$(rustc -V); $(cargo -V); x86_64-linux-musl-gcc $(x86_64-linux-musl-gcc -dumpfullversion)"

if [ -n "$rebuild" ]; then
  if wait "$rebuild_pid"; then rc=0; else rc=$?; fi
  echo "::group::rebuild in $rebuild/target"
  cat "$rebuild/build.log"
  echo "::endgroup::"
  if [ "$rc" -ne 0 ]; then
    annotate error reproducibility "the rebuild in $rebuild/target failed with exit $rc"
    exit 1
  fi
  if cmp -s "$out/SHA256SUMS" "$rebuild/out/SHA256SUMS"; then
    annotate notice reproducibility \
      "the rebuild from a clean $rebuild/target has the same SHA256SUMS"
  else
    for bin in "${BINS[@]}"; do
      first=$(grep -E "  $bin\$" "$out/SHA256SUMS")
      second=$(grep -E "  $bin\$" "$rebuild/out/SHA256SUMS")
      if [ "$first" != "$second" ]; then
        annotate error "reproducibility $bin" \
          "$bin differs: ${first%% *} in ./target, ${second%% *} in $rebuild/target"
      fi
    done
    status=1
  fi
fi
exit "$status"
