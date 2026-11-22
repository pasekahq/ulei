#!/usr/bin/env bash
# Resource budgets of shipped binaries, enforced in CI (ADR-0001, "Budgets").
# Skeleton (S0) measures the `ulei` CLI; every new binary or daemon adds its own section
# with the limits from its ADR. Do not raise a limit to make CI green.
#   ULEI_MAX_BINARY_BYTES  static release binary size        (default 512 KiB)
#   ULEI_MAX_RSS_KB        peak RSS of `ulei --version`      (default 1024 kB)
#   ULEI_MAX_START_US      median cold start of `--version`  (default 3000 us)
#   ULEI_MAX_SYSCALLS      syscalls of `ulei --version`      (default 40), 0 file opens
# The `ulei-inbox` section follows ADR-0007 section 4 and ADR-0036 section 9; ci/inbox_load.py
# drives the daemon on a synthetic ulei.toml and a fresh root on the runner disk (/var/tmp, never
# tmpfs), and measures the idle of `serve` with `inbox.deliver` on an enabled root with nothing
# queued, against a fake C6 socket:
#   ULEI_INBOX_MAX_BINARY_BYTES   static release binary size               (default 2560 KiB)
#   ULEI_INBOX_MAX_RSS_KB         VmHWM after 10^4 publish-claim-start-ack
#                                 and of `serve` with `inbox.deliver`
#                                 after 10 s idle                          (default 4096 kB)
#   ULEI_INBOX_MAX_READY_US       median spawn to the first 200 of `health`
#                                 over restarts on an initialised root     (default 10000 us)
#   ULEI_INBOX_MAX_IDLE_WAKEUPS   voluntary context switches of all threads
#                                 of `serve` with `inbox.deliver`
#                                 in 10 s idle                             (default 0)
#   ULEI_INBOX_MAX_PUBLISH_P99_US p99 publish, connect to last byte        (default 50000 us)
#   ULEI_INBOX_MAX_WRITE_BYTES    wchar over the cycles per event          (default 128 KiB)
# The first start on a fresh root is printed without a limit (ADR-0007 section 4).
# ci/status_run.py runs `ulei status` once against stdlib fake C1, C6 and C4G sockets, all up
# (ADR-0040 section 6): its syscalls are printed without a limit; it opens exactly 1 file
# (ulei.toml) and its peak RSS is held to ULEI_INBOX_MAX_RSS_KB.
# The `ulei-engine` section follows ADR-0010 section 4; ci/engine_idle.py holds `run` blocked
# on a silent fake gateway, then `serve` holding one `inbox_turns`, on the same runner disk:
#   ULEI_ENGINE_MAX_BINARY_BYTES  static release binary size               (default 4096 KiB)
#   ULEI_ENGINE_MAX_RSS_KB        VmHWM of `run` blocked on the gateway
#                                 and of `serve` idle                      (default 4096 kB)
#   ULEI_ENGINE_MAX_IDLE_WAKEUPS  voluntary context switches in 10 s idle
#                                 of `run` and of all `serve` threads      (default 0)
#   ulei-engine must not contain TLS code (ADR-0015 section 2).
# ci/session_search_run.py then fills a fresh journal through C6 `submit_turn` of `serve` with
# 100 sessions of 100 messages and has the fake gateway ask for 200 `session_search` calls in
# turn discovery, scroll and read; a call's latency runs from the gateway's answer carrying the
# call to the engine's next request (ADR-0051 section 7). The fill time and the p50 of each shape
# are printed without a limit:
#   ULEI_ENGINE_MAX_SESSION_SEARCH_P50_US p50 session_search latency      (default 20000 us)
# The `ulei-web` section follows ADR-0015 section 2; ci/web_rss.py runs one `web_extract` on a
# build with the `web-loopback-http` test switch against a loopback fake Exa:
#   ULEI_WEB_MAX_BINARY_BYTES     static release binary size               (default 2048 KiB)
#   ULEI_WEB_MAX_RSS_KB           peak RSS of a web_extract whose Exa reply
#                                 is near the 16 MiB cap                   (default 81920 kB)
# The `ulei-tg` section follows ADR-0017 section 4; ci/tg_idle.py holds the channel blocked in
# `getUpdates` on a loopback fake Bot API that never answers, on the same runner disk:
#   ULEI_TG_MAX_BINARY_BYTES      static release binary size               (default 2560 KiB)
#   ULEI_TG_MAX_RSS_KB            VmHWM blocked in getUpdates              (default 4096 kB)
#   ULEI_TG_MAX_IDLE_WAKEUPS      voluntary context switches of all threads
#                                 in 10 s idle                             (default 0)
# The `ulei-garden` section follows ADR-0047 section 7; ci/garden_run.py fills a fresh store on
# the same runner disk with 2,000 fictional facts through C7 `propose` into the seedbed and the
# night path `seedbed_take` and `seedbed_done`, the embedder off, misses
# published to a fake C1 socket, and drives `serve` on the filled store:
#   ULEI_GARDEN_MAX_BINARY_BYTES    static release binary size             (default 2560 KiB)
#   ULEI_GARDEN_MAX_RSS_KB          VmHWM after 10^4 pointers, search
#                                   and propose                            (default 4096 kB)
#   ULEI_GARDEN_MAX_READY_US        median spawn to the first 200 of
#                                   `health` over restarts on the filled
#                                   store                                  (default 10000 us)
#   ULEI_GARDEN_MAX_IDLE_WAKEUPS    voluntary context switches of all
#                                   threads in 10 s idle                   (default 0)
#   ULEI_GARDEN_MAX_POINTERS_P50_US p50 pointers, connect to last byte     (default 10000 us)
#   ULEI_GARDEN_MAX_SEARCH_P50_US   p50 search, connect to last byte       (default 20000 us)
#   ULEI_GARDEN_MAX_WRITE_BYTES     wchar per fact of the fill             (default 128 KiB)
# The p50 of a propose into the seedbed is printed without a limit.
# The `ulei-garden-embed` section follows ADR-0047 section 7: the helper's glibc build with
# `onnx`, the one binary checked to be dynamically linked, against glibc's own libraries only:
#   ULEI_GARDEN_EMBED_MAX_BINARY_BYTES release binary size; `ldd` lists only libc, libm,
#                                   ld-linux, libgcc_s and the vDSO      (default 2048 KiB)
# Each section ends with one `::notice` annotation of its figures and budgets, which the public
# check-runs annotations API serves without the job log.
# Usage: ci/budgets.sh [ulei [ulei-inbox [ulei-engine [ulei-web [ulei-web-loopback [ulei-tg
#   [ulei-garden [ulei-garden-embed]]]]]]]]
#   (ulei-inbox, ulei-engine, ulei-web, ulei-tg and ulei-garden default to siblings of ulei;
#   ulei-web-loopback to target/web-loopback/x86_64-unknown-linux-musl/release/ulei-web;
#   ulei-garden-embed to target/x86_64-unknown-linux-gnu/release/ulei-garden-embed)
set -euo pipefail

bin="${1:-target/x86_64-unknown-linux-musl/release/ulei}"
max_bytes="${ULEI_MAX_BINARY_BYTES:-524288}"
max_rss_kb="${ULEI_MAX_RSS_KB:-1024}"
max_start_us="${ULEI_MAX_START_US:-3000}"
max_syscalls="${ULEI_MAX_SYSCALLS:-40}"
fail=0
notice() { echo "::notice title=$1 budgets::$2"; }

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
notice ulei "binary $size B (budget $max_bytes B); --version peak RSS $rss kB (budget $max_rss_kb kB); cold start median $start us (budget $max_start_us us); syscalls ${calls:-unchecked} (budget $max_syscalls)"

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

work=$(mktemp -d -p /var/tmp ulei-budgets.XXXXXX)
trap 'rm -rf "$work"' EXIT
fs_type=$(stat -f -c %T "$work")
echo "daemon work directory: $work on $fs_type"
case "$fs_type" in
  tmpfs | ramfs) echo "::error::$work is on $fs_type, the daemon budgets need the runner disk"; exit 1 ;;
esac

if ! run=$(python3 "$(dirname "$0")/inbox_load.py" "$inbox" "$work"); then
  echo "::error::ulei-inbox load run failed"
  exit 1
fi
measured() { awk -v k="$1" '$1 == k {print $2}' <<<"$run"; }
cycles=$(measured cycles)
idle_seconds=$(measured idle_seconds)

ready=$(measured ready_us)
echo "ulei-inbox ready: median ${ready} us of $(measured ready_restarts) restarts on the initialised root $(measured ready_root), spawn to the first 200 of health (budget ${inbox_max_ready_us} us)"
if [ "$ready" -gt "$inbox_max_ready_us" ]; then echo "::error::ulei-inbox ready ${ready} us > ${inbox_max_ready_us} us"; fail=1; fi

echo "ulei-inbox first start: median $(measured first_start_us) us of $(measured first_starts) fresh roots, spawn to the first 200 of health (no budget)"

hwm=$(measured vmhwm_kb)
echo "ulei-inbox peak RSS (VmHWM) after ${cycles} cycles: ${hwm} kB (budget ${inbox_max_rss_kb} kB)"
if [ "$hwm" -gt "$inbox_max_rss_kb" ]; then echo "::error::ulei-inbox peak RSS ${hwm} kB > ${inbox_max_rss_kb} kB"; fail=1; fi

wakeups=$(measured idle_wakeups)
echo "ulei-inbox voluntary context switches of all $(measured idle_threads) threads of serve with inbox.deliver in ${idle_seconds} s idle: ${wakeups} (budget ${inbox_max_idle_wakeups})"
if [ "$wakeups" -gt "$inbox_max_idle_wakeups" ]; then echo "::error::ulei-inbox woke ${wakeups} times in ${idle_seconds} s idle"; fail=1; fi
deliver_hwm=$(measured deliver_vmhwm_kb)
echo "ulei-inbox peak RSS (VmHWM) of serve with inbox.deliver after ${idle_seconds} s idle: ${deliver_hwm} kB (budget ${inbox_max_rss_kb} kB)"
if [ "$deliver_hwm" -gt "$inbox_max_rss_kb" ]; then echo "::error::ulei-inbox serve with inbox.deliver peak RSS ${deliver_hwm} kB > ${inbox_max_rss_kb} kB"; fail=1; fi

p50=$(measured publish_p50_us)
p99=$(measured publish_p99_us)
echo "ulei-inbox publish p99 over ${cycles} connections: ${p99} us, p50 ${p50} us (budget p99 ${inbox_max_p99_us} us)"
if [ "$p99" -gt "$inbox_max_p99_us" ]; then echo "::error::ulei-inbox publish p99 ${p99} us > ${inbox_max_p99_us} us"; fail=1; fi

written=$(measured write_bytes_per_event)
echo "ulei-inbox write bytes per event (wchar over ${cycles} cycles): ${written} B (budget ${inbox_max_write_bytes} B)"
if [ "$written" -gt "$inbox_max_write_bytes" ]; then echo "::error::ulei-inbox writes ${written} B per event > ${inbox_max_write_bytes} B"; fail=1; fi
if ! status_run=$(python3 "$(dirname "$0")/status_run.py" "$bin" "$work"); then
  echo "::error::ulei status run failed"
  exit 1
fi
status_measured() { awk -v k="$1" '$1 == k {print $2}' <<<"$status_run"; }
status_calls=$(status_measured status_syscalls)
status_opens=$(status_measured status_opens)
status_rss=$(status_measured status_rss_kb)
echo "ulei status against fake daemons: syscalls ${status_calls} (no budget), file opens ${status_opens} (budget 1), peak RSS ${status_rss} kB (budget ${inbox_max_rss_kb} kB)"
if [ "$status_opens" -ne 1 ]; then echo "::error::ulei status opened ${status_opens} files, expected 1"; fail=1; fi
if [ "$status_rss" -gt "$inbox_max_rss_kb" ]; then echo "::error::ulei status peak RSS ${status_rss} kB > ${inbox_max_rss_kb} kB"; fail=1; fi
notice "ulei status" "syscalls $status_calls; file opens $status_opens (budget 1); peak RSS $status_rss kB (budget $inbox_max_rss_kb kB)"
notice ulei-inbox "binary $size B (budget $inbox_max_bytes B); VmHWM after $cycles cycles $hwm kB (budget $inbox_max_rss_kb kB); ready median $ready us (budget $inbox_max_ready_us us); idle wakeups $wakeups (budget $inbox_max_idle_wakeups); serve with inbox.deliver idle VmHWM $deliver_hwm kB (budget $inbox_max_rss_kb kB); publish p99 $p99 us (budget $inbox_max_p99_us us); write $written B per event (budget $inbox_max_write_bytes B)"

engine="${3:-$(dirname "$bin")/ulei-engine}"
engine_max_bytes="${ULEI_ENGINE_MAX_BINARY_BYTES:-4194304}"
engine_max_rss_kb="${ULEI_ENGINE_MAX_RSS_KB:-4096}"
engine_max_idle_wakeups="${ULEI_ENGINE_MAX_IDLE_WAKEUPS:-0}"
engine_max_search_us="${ULEI_ENGINE_MAX_SESSION_SEARCH_P50_US:-20000}"

size=$(stat -c %s "$engine")
echo "binary: $engine $size bytes (budget $engine_max_bytes)"
if [ "$size" -gt "$engine_max_bytes" ]; then echo "::error::ulei-engine binary size $size > $engine_max_bytes"; fail=1; fi
if file "$engine" | grep -q 'dynamically linked'; then echo "::error::ulei-engine is dynamically linked, expected static"; fail=1; fi

if ! run=$(python3 "$(dirname "$0")/engine_idle.py" "$engine" "$work"); then
  echo "::error::ulei-engine idle run failed"
  exit 1
fi
idle_seconds=$(measured idle_seconds)

hwm=$(measured vmhwm_kb)
echo "ulei-engine peak RSS (VmHWM) blocked on the gateway after turn request $(measured request_id): ${hwm} kB (budget ${engine_max_rss_kb} kB)"
if [ "$hwm" -gt "$engine_max_rss_kb" ]; then echo "::error::ulei-engine peak RSS ${hwm} kB > ${engine_max_rss_kb} kB"; fail=1; fi

wakeups=$(measured idle_wakeups)
echo "ulei-engine voluntary context switches in ${idle_seconds} s idle: ${wakeups} (budget ${engine_max_idle_wakeups})"
if [ "$wakeups" -gt "$engine_max_idle_wakeups" ]; then echo "::error::ulei-engine woke ${wakeups} times in ${idle_seconds} s idle"; fail=1; fi

serve_hwm=$(measured serve_vmhwm_kb)
echo "ulei-engine serve peak RSS (VmHWM) holding an inbox_turns: ${serve_hwm} kB (budget ${engine_max_rss_kb} kB)"
if [ "$serve_hwm" -gt "$engine_max_rss_kb" ]; then echo "::error::ulei-engine serve peak RSS ${serve_hwm} kB > ${engine_max_rss_kb} kB"; fail=1; fi

serve_wakeups=$(measured serve_idle_wakeups)
echo "ulei-engine serve voluntary context switches in ${idle_seconds} s idle: ${serve_wakeups} (budget ${engine_max_idle_wakeups})"
if [ "$serve_wakeups" -gt "$engine_max_idle_wakeups" ]; then echo "::error::ulei-engine serve woke ${serve_wakeups} times in ${idle_seconds} s idle"; fail=1; fi
if grep -aq rustls "$engine"; then echo "::error::ulei-engine contains rustls code"; fail=1; else echo "ulei-engine contains no rustls code"; fi
notice ulei-engine "binary $size B (budget $engine_max_bytes B); idle VmHWM $hwm kB (budget $engine_max_rss_kb kB); idle wakeups $wakeups in $idle_seconds s (budget $engine_max_idle_wakeups); serve idle VmHWM $serve_hwm kB (budget $engine_max_rss_kb kB); serve idle wakeups $serve_wakeups in $idle_seconds s (budget $engine_max_idle_wakeups)"

if ! run=$(python3 "$(dirname "$0")/session_search_run.py" "$engine" "$work"); then
  echo "::error::ulei-engine session_search run failed"
  exit 1
fi
fill="$(measured fill_sessions) sessions of $(measured fill_messages) messages in $(measured fill_turns) submit_turn turns, $(measured fill_seconds) s"
echo "ulei-engine session_search journal: $fill (no budget)"
search_calls=$(measured search_calls)
shapes="discovery p50 $(measured discovery_p50_us) us of $(measured discovery_calls), scroll p50 $(measured scroll_p50_us) us of $(measured scroll_calls), read p50 $(measured read_p50_us) us of $(measured read_calls)"
search=$(measured search_p50_us)
echo "ulei-engine session_search p50 over ${search_calls} calls, gateway answer to the next request: ${search} us (budget ${engine_max_search_us} us); $shapes (no budget)"
if [ "$search" -gt "$engine_max_search_us" ]; then echo "::error::ulei-engine session_search p50 ${search} us > ${engine_max_search_us} us"; fail=1; fi
notice "ulei-engine session_search" "p50 $search us over $search_calls calls (budget $engine_max_search_us us); $shapes; journal $fill"

web="${4:-$(dirname "$bin")/ulei-web}"
web_loopback="${5:-target/web-loopback/x86_64-unknown-linux-musl/release/ulei-web}"
web_max_bytes="${ULEI_WEB_MAX_BINARY_BYTES:-2097152}"
web_max_rss_kb="${ULEI_WEB_MAX_RSS_KB:-81920}"

size=$(stat -c %s "$web")
echo "binary: $web $size bytes (budget $web_max_bytes)"
if [ "$size" -gt "$web_max_bytes" ]; then echo "::error::ulei-web binary size $size > $web_max_bytes"; fail=1; fi
if file "$web" | grep -q 'dynamically linked'; then echo "::error::ulei-web is dynamically linked, expected static"; fail=1; fi

if ! run=$(python3 "$(dirname "$0")/web_rss.py" "$web_loopback" "$work"); then
  echo "::error::ulei-web RSS run failed"
  exit 1
fi
rss=$(measured peak_rss_kb)
echo "ulei-web peak RSS of one web_extract with a $(measured reply_bytes)-byte Exa reply: ${rss} kB (budget ${web_max_rss_kb} kB)"
if [ "$rss" -gt "$web_max_rss_kb" ]; then echo "::error::ulei-web peak RSS ${rss} kB > ${web_max_rss_kb} kB"; fail=1; fi
notice ulei-web "binary $size B (budget $web_max_bytes B); web_extract peak RSS $rss kB (budget $web_max_rss_kb kB)"

tg="${6:-$(dirname "$bin")/ulei-tg}"
tg_max_bytes="${ULEI_TG_MAX_BINARY_BYTES:-2621440}"
tg_max_rss_kb="${ULEI_TG_MAX_RSS_KB:-4096}"
tg_max_idle_wakeups="${ULEI_TG_MAX_IDLE_WAKEUPS:-0}"

size=$(stat -c %s "$tg")
echo "binary: $tg $size bytes (budget $tg_max_bytes)"
if [ "$size" -gt "$tg_max_bytes" ]; then echo "::error::ulei-tg binary size $size > $tg_max_bytes"; fail=1; fi
if file "$tg" | grep -q 'dynamically linked'; then echo "::error::ulei-tg is dynamically linked, expected static"; fail=1; fi

if ! run=$(python3 "$(dirname "$0")/tg_idle.py" "$tg" "$work"); then
  echo "::error::ulei-tg idle run failed"
  exit 1
fi
idle_seconds=$(measured idle_seconds)

hwm=$(measured vmhwm_kb)
echo "ulei-tg peak RSS (VmHWM) blocked in getUpdates: ${hwm} kB (budget ${tg_max_rss_kb} kB)"
if [ "$hwm" -gt "$tg_max_rss_kb" ]; then echo "::error::ulei-tg peak RSS ${hwm} kB > ${tg_max_rss_kb} kB"; fail=1; fi

wakeups=$(measured idle_wakeups)
echo "ulei-tg voluntary context switches of $(measured threads) threads in ${idle_seconds} s idle: ${wakeups} (budget ${tg_max_idle_wakeups})"
if [ "$wakeups" -gt "$tg_max_idle_wakeups" ]; then echo "::error::ulei-tg woke ${wakeups} times in ${idle_seconds} s idle"; fail=1; fi
notice ulei-tg "binary $size B (budget $tg_max_bytes B); idle VmHWM $hwm kB (budget $tg_max_rss_kb kB); idle wakeups $wakeups in $idle_seconds s (budget $tg_max_idle_wakeups)"

garden="${7:-$(dirname "$bin")/ulei-garden}"
garden_max_bytes="${ULEI_GARDEN_MAX_BINARY_BYTES:-2621440}"
garden_max_rss_kb="${ULEI_GARDEN_MAX_RSS_KB:-4096}"
garden_max_ready_us="${ULEI_GARDEN_MAX_READY_US:-10000}"
garden_max_idle_wakeups="${ULEI_GARDEN_MAX_IDLE_WAKEUPS:-0}"
garden_max_pointers_us="${ULEI_GARDEN_MAX_POINTERS_P50_US:-10000}"
garden_max_search_us="${ULEI_GARDEN_MAX_SEARCH_P50_US:-20000}"
garden_max_write_bytes="${ULEI_GARDEN_MAX_WRITE_BYTES:-131072}"

size=$(stat -c %s "$garden")
echo "binary: $garden $size bytes (budget $garden_max_bytes)"
if [ "$size" -gt "$garden_max_bytes" ]; then echo "::error::ulei-garden binary size $size > $garden_max_bytes"; fail=1; fi
if file "$garden" | grep -q 'dynamically linked'; then echo "::error::ulei-garden is dynamically linked, expected static"; fail=1; fi

if ! run=$(python3 "$(dirname "$0")/garden_run.py" "$garden" "$work"); then
  echo "::error::ulei-garden run failed"
  exit 1
fi
requests=$(measured requests)
idle_seconds=$(measured idle_seconds)

written=$(measured write_bytes_per_fact)
echo "ulei-garden write bytes per fact (wchar over $(measured fill_facts) proposes and their seedbed_done filling a fresh store): ${written} B (budget ${garden_max_write_bytes} B)"
if [ "$written" -gt "$garden_max_write_bytes" ]; then echo "::error::ulei-garden writes ${written} B per fact > ${garden_max_write_bytes} B"; fail=1; fi

ready=$(measured ready_us)
echo "ulei-garden ready: median ${ready} us of $(measured ready_restarts) restarts on the filled store, spawn to the first 200 of health (budget ${garden_max_ready_us} us)"
if [ "$ready" -gt "$garden_max_ready_us" ]; then echo "::error::ulei-garden ready ${ready} us > ${garden_max_ready_us} us"; fail=1; fi

pointers=$(measured pointers_p50_us)
echo "ulei-garden pointers p50 over $(measured pointers_requests) connections: ${pointers} us, $(measured pointers_answered) answered with pointers (budget ${garden_max_pointers_us} us)"
if [ "$pointers" -gt "$garden_max_pointers_us" ]; then echo "::error::ulei-garden pointers p50 ${pointers} us > ${garden_max_pointers_us} us"; fail=1; fi

search=$(measured search_p50_us)
echo "ulei-garden search p50: ${search} us (budget ${garden_max_search_us} us)"
if [ "$search" -gt "$garden_max_search_us" ]; then echo "::error::ulei-garden search p50 ${search} us > ${garden_max_search_us} us"; fail=1; fi

propose=$(measured propose_p50_us)
echo "ulei-garden propose p50 into the seedbed: ${propose} us (no budget)"

hwm=$(measured vmhwm_kb)
echo "ulei-garden peak RSS (VmHWM) after ${requests} requests: ${hwm} kB (budget ${garden_max_rss_kb} kB)"
if [ "$hwm" -gt "$garden_max_rss_kb" ]; then echo "::error::ulei-garden peak RSS ${hwm} kB > ${garden_max_rss_kb} kB"; fail=1; fi

wakeups=$(measured idle_wakeups)
echo "ulei-garden voluntary context switches of all $(measured idle_threads) threads in ${idle_seconds} s idle after $(measured published_misses) misses published: ${wakeups} (budget ${garden_max_idle_wakeups})"
if [ "$wakeups" -gt "$garden_max_idle_wakeups" ]; then echo "::error::ulei-garden woke ${wakeups} times in ${idle_seconds} s idle"; fail=1; fi
notice ulei-garden "binary $size B (budget $garden_max_bytes B); VmHWM after $requests requests $hwm kB (budget $garden_max_rss_kb kB); ready median $ready us (budget $garden_max_ready_us us); idle wakeups $wakeups in $idle_seconds s (budget $garden_max_idle_wakeups); pointers p50 $pointers us (budget $garden_max_pointers_us us); search p50 $search us (budget $garden_max_search_us us); write $written B per fact (budget $garden_max_write_bytes B); propose p50 $propose us (no budget)"

embed="${8:-target/x86_64-unknown-linux-gnu/release/ulei-garden-embed}"
embed_max_bytes="${ULEI_GARDEN_EMBED_MAX_BINARY_BYTES:-2097152}"

size=$(stat -c %s "$embed")
echo "binary: $embed $size bytes (budget $embed_max_bytes)"
if [ "$size" -gt "$embed_max_bytes" ]; then echo "::error::ulei-garden-embed binary size $size > $embed_max_bytes"; fail=1; fi
if ! file "$embed" | grep -q 'dynamically linked'; then echo "::error::ulei-garden-embed is not dynamically linked"; fail=1; fi
if ! libs=$(ldd "$embed" | awk '{print $1}' | xargs -n1 basename | sort | tr '\n' ' '); then
  echo "::error::ldd cannot list the libraries of ulei-garden-embed"
  exit 1
fi
echo "ulei-garden-embed dynamic libraries: $libs"
for lib in $libs; do
  case "$lib" in
    linux-vdso.so.1 | libc.so.6 | libm.so.6 | ld-linux-x86-64.so.2 | libgcc_s.so.1) ;;
    *) echo "::error::ulei-garden-embed links $lib, only libc, libm, ld-linux and libgcc_s are allowed"; fail=1 ;;
  esac
done
notice ulei-garden-embed "binary $size B (budget $embed_max_bytes B); dynamic libraries ${libs% }"
exit "$fail"
