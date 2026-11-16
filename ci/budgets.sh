#!/usr/bin/env bash
# Resource budgets of shipped binaries, enforced in CI (ADR-0001, "Budgets").
# Skeleton (S0) measures the `ulei` CLI; every new binary or daemon adds its own section
# with the limits from its ADR. Do not raise a limit to make CI green.
#   ULEI_MAX_BINARY_BYTES  static release binary size        (default 512 KiB)
#   ULEI_MAX_RSS_KB        peak RSS of `ulei --version`      (default 1024 kB)
#   ULEI_MAX_START_US      median cold start of `--version`  (default 3000 us)
#   ULEI_MAX_SYSCALLS      syscalls of `ulei --version`      (default 40), 0 file opens
# The `ulei-inbox` section follows ADR-0007 section 4; ci/inbox_load.py drives the daemon on a
# fresh root on the runner disk (/var/tmp, never tmpfs):
#   ULEI_INBOX_MAX_BINARY_BYTES   static release binary size               (default 2560 KiB)
#   ULEI_INBOX_MAX_RSS_KB         VmHWM after 10^4 publish-claim-start-ack (default 4096 kB)
#   ULEI_INBOX_MAX_READY_US       spawn to the first 200 of `health`       (default 10000 us)
#   ULEI_INBOX_MAX_IDLE_WAKEUPS   voluntary context switches in 10 s idle  (default 0)
#   ULEI_INBOX_MAX_PUBLISH_P99_US p99 publish, connect to last byte        (default 50000 us)
#   ULEI_INBOX_MAX_WRITE_BYTES    wchar over the cycles per event          (default 128 KiB)
# Usage: ci/budgets.sh [path/to/ulei [path/to/ulei-inbox]]
#   (ulei-inbox defaults to the sibling of ulei)
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

inbox="${2:-$(dirname "$bin")/ulei-inbox}"
inbox_max_bytes="${ULEI_INBOX_MAX_BINARY_BYTES:-2621440}"
inbox_max_rss_kb="${ULEI_INBOX_MAX_RSS_KB:-4096}"
inbox_max_ready_us="${ULEI_INBOX_MAX_READY_US:-10000}"
inbox_max_idle_wakeups="${ULEI_INBOX_MAX_IDLE_WAKEUPS:-0}"
inbox_max_p99_us="${ULEI_INBOX_MAX_PUBLISH_P99_US:-50000}"
inbox_max_write_bytes="${ULEI_INBOX_MAX_WRITE_BYTES:-131072}"

size=$(stat -c %s "$inbox")
echo "binary: $inbox $size bytes (budget $inbox_max_bytes)"
if [ "$size" -gt "$inbox_max_bytes" ]; then echo "::error::ulei-inbox binary size $size > $inbox_max_bytes"; fail=1; fi
if file "$inbox" | grep -q 'dynamically linked'; then echo "::error::ulei-inbox is dynamically linked, expected static"; fail=1; fi

work=$(mktemp -d -p /var/tmp ulei-inbox-budgets.XXXXXX)
trap 'rm -rf "$work"' EXIT
fs_type=$(stat -f -c %T "$work")
echo "ulei-inbox work directory: $work on $fs_type"
case "$fs_type" in
  tmpfs | ramfs) echo "::error::$work is on $fs_type, the ulei-inbox budgets need the runner disk"; exit 1 ;;
esac

if ! run=$(python3 "$(dirname "$0")/inbox_load.py" "$inbox" "$work"); then
  echo "::error::ulei-inbox load run failed"
  exit 1
fi
measured() { awk -v k="$1" '$1 == k {print $2}' <<<"$run"; }
cycles=$(measured cycles)
idle_seconds=$(measured idle_seconds)

ready=$(measured ready_us)
echo "ulei-inbox ready: ${ready} us from spawn to the first 200 of health (budget ${inbox_max_ready_us} us)"
if [ "$ready" -gt "$inbox_max_ready_us" ]; then echo "::error::ulei-inbox ready ${ready} us > ${inbox_max_ready_us} us"; fail=1; fi

hwm=$(measured vmhwm_kb)
echo "ulei-inbox peak RSS (VmHWM) after ${cycles} cycles: ${hwm} kB (budget ${inbox_max_rss_kb} kB)"
if [ "$hwm" -gt "$inbox_max_rss_kb" ]; then echo "::error::ulei-inbox peak RSS ${hwm} kB > ${inbox_max_rss_kb} kB"; fail=1; fi

written=$(measured write_bytes_per_event)
echo "ulei-inbox write bytes per event (wchar over ${cycles} cycles): ${written} B (budget ${inbox_max_write_bytes} B)"
if [ "$written" -gt "$inbox_max_write_bytes" ]; then echo "::error::ulei-inbox writes ${written} B per event > ${inbox_max_write_bytes} B"; fail=1; fi

p50=$(measured publish_p50_us)
p99=$(measured publish_p99_us)
echo "ulei-inbox publish p99 over ${cycles} connections: ${p99} us, p50 ${p50} us (budget p99 ${inbox_max_p99_us} us)"
if [ "$p99" -gt "$inbox_max_p99_us" ]; then echo "::error::ulei-inbox publish p99 ${p99} us > ${inbox_max_p99_us} us"; fail=1; fi

wakeups=$(measured idle_wakeups)
echo "ulei-inbox voluntary context switches in ${idle_seconds} s idle: ${wakeups} (budget ${inbox_max_idle_wakeups})"
if [ "$wakeups" -gt "$inbox_max_idle_wakeups" ]; then echo "::error::ulei-inbox woke ${wakeups} times in ${idle_seconds} s idle"; fail=1; fi
exit "$fail"
