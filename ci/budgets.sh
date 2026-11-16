#!/usr/bin/env bash
# Resource budgets of shipped binaries, enforced in CI (ADR-0001, "Budgets").
# Skeleton (S0) measures the `ulei` CLI; every new binary or daemon adds its own section
# with the limits from its ADR. Do not raise a limit to make CI green.
#   ULEI_MAX_BINARY_BYTES  static release binary size        (default 512 KiB)
#   ULEI_MAX_RSS_KB        peak RSS of `ulei --version`      (default 1024 kB)
#   ULEI_MAX_START_US      median cold start of `--version`  (default 3000 us)
#   ULEI_MAX_SYSCALLS      syscalls of `ulei --version`      (default 40), 0 file opens
# Usage: ci/budgets.sh [path/to/ulei]
set -euo pipefail

bin="${1:-target/x86_64-unknown-linux-musl/release/ulei}"
max_bytes="${ULEI_MAX_BINARY_BYTES:-524288}"
max_rss_kb="${ULEI_MAX_RSS_KB:-1024}"
max_start_us="${ULEI_MAX_START_US:-3000}"
max_syscalls="${ULEI_MAX_SYSCALLS:-40}"
fail=0

size=$(stat -c %s "$bin")
echo "binary: $bin $size bytes (budget $max_bytes)"
if [ "$size" -gt "$max_bytes" ]; then echo "::error::binary size $size > $max_bytes"; fail=1; fi
if file "$bin" | grep -q 'dynamically linked'; then echo "::error::binary is dynamically linked, expected static"; fail=1; fi

rss=$( { /usr/bin/time -f '%M' "$bin" --version >/dev/null; } 2>&1 | tail -1)
echo "peak RSS of --version: ${rss} kB (budget ${max_rss_kb} kB)"
if [ "$rss" -gt "$max_rss_kb" ]; then echo "::error::peak RSS ${rss} kB > ${max_rss_kb} kB"; fail=1; fi

start=$(python3 - "$bin" <<'PY'
import subprocess, sys, time
t = []
for _ in range(300):
    a = time.perf_counter()
    subprocess.run([sys.argv[1], "--version"], stdout=subprocess.DEVNULL, check=True)
    t.append(time.perf_counter() - a)
t.sort()
print(int(t[len(t) // 2] * 1e6))
PY
)
echo "cold start of --version: median ${start} us incl. spawn (budget ${max_start_us} us)"
if [ "$start" -gt "$max_start_us" ]; then echo "::error::cold start ${start} us > ${max_start_us} us"; fail=1; fi

if command -v strace >/dev/null; then
  trace=$(mktemp)
  strace -f -c -o "$trace" "$bin" --version >/dev/null
  calls=$(awk '$NF == "total" {print $4}' "$trace")
  opens=$(awk '$NF ~ /^open/ {s += $4} END {print s + 0}' "$trace")
  echo "syscalls of --version: ${calls} (budget ${max_syscalls}), file opens: ${opens} (budget 0)"
  if [ "$calls" -gt "$max_syscalls" ]; then echo "::error::syscalls ${calls} > ${max_syscalls}"; fail=1; fi
  if [ "$opens" -gt 0 ]; then echo "::error::--version opened ${opens} files"; fail=1; fi
  rm -f "$trace"
else
  echo "::warning::strace not installed; syscall budget not checked"
fi
exit "$fail"
