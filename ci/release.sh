#!/usr/bin/env bash
# Static x86_64-unknown-linux-musl release of the shipped binaries and connectors (ADR-0042
# section 10, ADR-0056 section 6).
#   ci/release.sh OUT_DIR [REBUILD_DIR]
# Builds into ./target with the reproducibility settings of section 10, copies the binaries into
# OUT_DIR and the test connector into OUT_DIR/connectors/test (its binary and the manifest of
# deploy/connectors/test), writes OUT_DIR/SHA256SUMS over exactly these files by relative path,
# and fails unless each binary is static, OUT_DIR holds no other file and the released
# `ulei-connectors list` accepts the manifest. With REBUILD_DIR, which must not exist, it builds a
# second time from a clean REBUILD_DIR/target at the same time and fails, naming the file, unless
# both SHA256SUMS are identical. Results are GitHub annotations: one notice with every binary's
# linkage, one with SHA256SUMS, one with the connector tree, one with the toolchain, and one with
# the comparison; an error per failed file.
set -euo pipefail

TRIPLE=x86_64-unknown-linux-musl
BINS=(ulei ulei-inbox ulei-engine ulei-web ulei-tg ulei-connectors)
TEST=connectors/test
EXECUTABLES=("${BINS[@]}" "$TEST/ulei-connector-test")
FILES=("${EXECUTABLES[@]}" "$TEST/connector.json")

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
  cargo_release -p ulei-connector-test
  for name in "${BINS[@]}"; do
    install -m 0755 "$target_dir/$TRIPLE/release/$name" "$out_dir/$name"
  done
  mkdir -p "$out_dir/$TEST"
  install -m 0755 "$target_dir/$TRIPLE/release/ulei-connector-test" \
    "$out_dir/$TEST/ulei-connector-test"
  install -m 0644 "$root/deploy/$TEST/connector.json" "$out_dir/$TEST/connector.json"
  cd "$out_dir"
  sha256sum "${FILES[@]}" > SHA256SUMS
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
static=""
for bin in "${EXECUTABLES[@]}"; do
  desc=$(file -b "$out/$bin")
  case "$desc" in
    *interpreter* | *"dynamically linked"*)
      annotate error "static $bin" "$desc"; status=1 ;;
    *"static-pie linked"* | *"statically linked"*)
      static="$static$bin: $desc"$'\n' ;;
    *)
      annotate error "static $bin" "not a static executable: $desc"; status=1 ;;
  esac
done
if [ -n "$static" ]; then annotate notice "static binaries" "${static%$'\n'}"; fi
annotate notice SHA256SUMS "$(cat "$out/SHA256SUMS")"

extra=$(cd "$out" && find . ! -type d ! -path ./SHA256SUMS | sed 's|^\./||' | LC_ALL=C sort \
  | LC_ALL=C comm -23 - <(printf '%s\n' "${FILES[@]}" | LC_ALL=C sort))
# The runner's own discovery is the C11 check of the manifest: schema, patterns and contract major.
check=$(mktemp -d)
case "$out" in
  *\"* | *\\*) listed="the release path $out holds a quote or backslash"; listing=1 ;;
  *)
    printf '[connectors]\ndir = "%s"\nenabled = "test"\n' "$out/connectors" > "$check/ulei.toml"
    if listed=$("$out/ulei-connectors" list --config "$check/ulei.toml" 2>&1); then listing=0
    else listing=$?; fi ;;
esac
rm -rf "$check"
if [ -n "$extra" ]; then
  annotate error connectors "files in $out outside SHA256SUMS: ${extra//$'\n'/, }"
  status=1
elif [ "$listing" -ne 0 ]; then
  annotate error connectors \
    "ulei-connectors list refuses the released manifest (exit $listing): $listed"
  status=1
else
  annotate notice connectors "$(cd "$out" && find connectors ! -type d | LC_ALL=C sort)
no file outside SHA256SUMS; ulei-connectors list of the release accepts the manifest: $listed"
fi
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
      "the rebuild from a clean $rebuild/target has the same SHA256SUMS over its ${#FILES[@]} files"
  else
    for file in "${FILES[@]}"; do
      first=$(awk -v f="$file" '$2 == f { print $1 }' "$out/SHA256SUMS")
      second=$(awk -v f="$file" '$2 == f { print $1 }' "$rebuild/out/SHA256SUMS")
      if [ "$first" != "$second" ]; then
        annotate error "reproducibility $file" \
          "$file differs: $first in ./target, $second in $rebuild/target"
      fi
    done
    status=1
  fi
fi
exit "$status"
