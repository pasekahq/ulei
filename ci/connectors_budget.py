"""Driver of the `ulei-connectors` budgets (ADR-0056 section 8) on the static musl release binary.

``python3 ci/connectors_budget.py idle ULEI_CONNECTORS WORKDIR`` is the `ulei-connectors` section of
``ci/budgets.sh``: ``IDLE_CONNECTORS`` enabled connectors whose ``runner.json`` holds a start of
now, so none is due for an hour, a fake C10 answering the `sync` of each start, the median from
spawn to the ready line over ``READY_RESTARTS`` starts, and the voluntary context switches of
all threads of ``serve`` in ``IDLE_SECONDS``. It prints one ``<name> <value>`` line per
measurement.

``python3 ci/connectors_budget.py runs ULEI_CONNECTORS ULEI_CONNECTOR_TEST WORKDIR`` runs in job
``systemd-units``, which has the user manager the runs need: ``RUNS`` enabled test connectors,
never run, so all are due at start and ``serve`` runs them four at a time, each one run of
``ITEMS`` items into a fake C10. ``serve`` runs in a transient scope of the user manager, whose
cgroup holds it and its ``systemd-run`` children as ``ulei-connectors.service`` would. After the
``RUNS`` records it reads ``serve``'s VmHWM and wchar, which holds the writes of its reaped
children, and the scope's ``memory.peak``; while they run it samples the threads of ``serve``,
its children and the scope's ``pids.current``. Then a second ``serve`` runs one scripted MCP
connector whose single ``ulei_pull`` page holds ``LARGE_ITEMS`` items of ``LARGE_TEXT`` bytes and
one item over the cap after a ``content`` text over the cap, and its VmHWM is read after the run.
WORKDIR must lie on the file system of ULEI_CONNECTOR_TEST, which each connector's directory holds
as a hard link. Each value is one notice, a value over its limit one error, and any error makes the
exit status 1:
  ULEI_CONNECTORS_MAX_RSS_KB       VmHWM of serve after the runs
                                   and after the MCP page          (default 4096 kB)
  ULEI_CONNECTORS_MAX_WRITE_BYTES  wchar of serve per run          (default 16 KiB)
"""

from __future__ import annotations

import json
import os
import select
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

from annotations import annotate
from connectors_run import Bed, FakeArchive, mcp_server, run_records
from garden_run import READY_RESTARTS, alive, p50
from inbox_load import Failure, proc_field, stop, thread_switches

IDLE_CONNECTORS = 4
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY = b"ulei-connectors: ready: "
READY_TIMEOUT = 10.0
# Each connector and `sync` once within a run of the driver.
EVERY = 3600
RUNS = 100
# The items of one run of the test connector: the runner's C11 `limit`.
ITEMS = 100
RUNS_TIMEOUT = 300.0
SAMPLE_SECONDS = 0.02
MAX_RSS_KB = int(os.environ.get("ULEI_CONNECTORS_MAX_RSS_KB", "4096"))
MAX_WRITE_BYTES = int(os.environ.get("ULEI_CONNECTORS_MAX_WRITE_BYTES", "16384"))
TITLE = "ulei-connectors budgets"
# The MCP page: items of about 1 MiB each and one over the cap (C11 `x-stdio.max_line_bytes`).
LARGE_ITEMS = 10
LARGE_TEXT = 1_000_000
OVER_CAP_TEXT = 2 * 1024 * 1024
# Its answer to `ulei_pull` call 2, written by `/bin/sh` with `a` for every byte of a text; the id
# of the item over the cap follows its text.
LARGE_PAGE = r"""printf '{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"'
head -c OVER /dev/zero | tr '\0' a
printf '"}],"structuredContent":{"items":['
i=1
while [ $i -le COUNT ]; do
  printf '{"external_id":"large-%s","kind":"text/plain","bytes_b64":"","text":"' $i
  head -c LARGE /dev/zero | tr '\0' a
  printf '"},'
  i=$((i + 1))
done
printf '{"kind":"text/plain","bytes_b64":"","text":"'
head -c OVER /dev/zero | tr '\0' a
printf '","external_id":"over-1"}],"cursor":"c1","more":false}}}\n'""".replace(
    "OVER", str(OVER_CAP_TEXT)).replace("COUNT", str(LARGE_ITEMS)).replace("LARGE", str(LARGE_TEXT))
# `systemd-run --scope` executes the command in its own process, so the scope holds `serve`.
SCOPE = ("systemd-run", "--user", "--scope", "--quiet", "--unit=ulei-connectors-budget")


def spawn(
    runner: Path, config: Path, prefix: tuple[str, ...] = (),
) -> tuple[subprocess.Popen, float]:
    """A running ``serve`` of ``config`` started through ``prefix`` and the seconds from spawn
    to its ready line."""
    spawned = time.perf_counter()
    daemon = subprocess.Popen(
        [*prefix, str(runner), "serve", "--config", str(config)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        said = b""
        while not any(line.startswith(READY) for line in said.split(b"\n")[:-1]):
            left = spawned + READY_TIMEOUT - time.perf_counter()
            if left <= 0 or not select.select([daemon.stderr], [], [], left)[0]:
                raise Failure(f"no ready line within {READY_TIMEOUT} s: {said!r}")
            chunk = os.read(daemon.stderr.fileno(), 4096)
            if not chunk:
                raise Failure(f"serve exited with status {daemon.wait()} before ready: {said!r}")
            said += chunk
        return daemon, time.perf_counter() - spawned
    except BaseException:
        stop(daemon)
        raise


def idle(runner: Path, work: Path) -> dict[str, int]:
    bed = Bed(runner.parent, work / "connectors")
    bed.work.mkdir(mode=0o700)
    names = [f"idle{n}" for n in range(1, IDLE_CONNECTORS + 1)]
    for name in names:
        bed.connector(name, ["bin/ulei-connector-test"], every=EVERY)
        state = bed.state / name
        state.mkdir(parents=True)
        (state / "runner.json").write_text(
            json.dumps({"cursor": None, "last_start": time.time()}), encoding="utf-8",
        )
    bed.write(names, {}, sync_every=EVERY)
    archive = FakeArchive(bed.c10)
    try:
        samples = []
        for _ in range(READY_RESTARTS):
            daemon, ready = spawn(runner, bed.config)
            stop(daemon)
            samples.append(ready)
        daemon, _ = spawn(runner, bed.config)
        try:
            time.sleep(SETTLE_SECONDS)
            before = thread_switches(daemon.pid)
            time.sleep(IDLE_SECONDS)
            after = thread_switches(daemon.pid)
            alive(daemon, "while idle")
        finally:
            stop(daemon)
    finally:
        archive.close()
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    if bed.records():
        raise Failure(f"{len(bed.records())} runs with nothing due")
    return {
        "ready_us": p50(samples),
        "ready_restarts": READY_RESTARTS,
        "connectors": IDLE_CONNECTORS,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
    }


def children(pid: int) -> list[int]:
    """The child processes of every thread of ``pid``."""
    found = []
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            found += [int(child) for child in (task / "children").read_text().split()]
        except FileNotFoundError:
            pass  # The thread ended between the listing and the read.
    return found


def cgroup(pid: int) -> Path:
    """The cgroup v2 directory of ``pid``."""
    for line in Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines():
        if line.startswith("0::"):
            return Path("/sys/fs/cgroup" + line.removeprefix("0::"))
    raise Failure(f"pid {pid} is in no cgroup v2")


class Peaks:
    """The most threads of ``serve``, children and tasks of its cgroup seen."""

    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.group = cgroup(pid)
        self.threads = 0
        self.children = 0
        self.tasks = 0

    def sample(self) -> None:
        self.threads = max(self.threads, len(list(Path(f"/proc/{self.pid}/task").iterdir())))
        self.children = max(self.children, len(children(self.pid)))
        current = (self.group / "pids.current").read_text(encoding="ascii")
        self.tasks = max(self.tasks, int(current))


def drain(daemon: subprocess.Popen, said: list[bytes]) -> None:
    """Keeps reading ``daemon``'s stderr into ``said`` until it closes, so a full pipe never
    blocks it."""
    while chunk := os.read(daemon.stderr.fileno(), 65536):
        said.append(chunk)


def mcp_page(runner: Path, work: Path) -> tuple[int, str]:
    """``serve``'s VmHWM after one run of the MCP page, and the run's text."""
    bed = Bed(runner.parent, work)
    work.mkdir(mode=0o700)
    program = bed.connector("mcp", ["bin/run.sh"], every=EVERY, transport="mcp") / "bin" / "run.sh"
    program.write_text(f"#!/bin/sh\n{mcp_server([LARGE_PAGE])}\n", encoding="utf-8")
    program.chmod(0o755)
    bed.write(["mcp"], {}, sync_every=EVERY)
    archive = FakeArchive(bed.c10)
    try:
        daemon, _ = spawn(runner, bed.config)
        said: list[bytes] = []
        threading.Thread(target=drain, args=(daemon, said), daemon=True).start()
        try:
            began = time.monotonic()
            while not run_records(bed.log):
                alive(daemon, "during the MCP run")
                if time.monotonic() - began > RUNS_TIMEOUT:
                    raise Failure(f"no MCP run record in {RUNS_TIMEOUT} s")
                time.sleep(SAMPLE_SECONDS)
            took = time.monotonic() - began
            hwm = proc_field(daemon.pid, "status", "VmHWM")
        finally:
            stop(daemon)
        record = bed.records()[0]
        stored = [p["external_id"] for p in archive.of("mcp")]
    finally:
        archive.close()
    want = [f"large-{n}" for n in range(1, LARGE_ITEMS + 1)]
    if record["outcome"] != "ok" or stored != want or record["too_large_ids"] != ["over-1"]:
        tail = b"".join(said).decode(errors="replace").splitlines()[-3:]
        raise Failure(
            f"MCP run outcome {record['outcome']}, puts {stored}, too_large_ids "
            f"{record['too_large_ids']}; stderr: {tail}"
        )
    return hwm, (
        f"VmHWM of serve after one MCP ulei_pull page of {LARGE_ITEMS} items of {LARGE_TEXT} "
        f"bytes and one of {OVER_CAP_TEXT} bytes after a content text of {OVER_CAP_TEXT} bytes, "
        f"run ok in {took:.1f} s with {len(stored)} stored and too_large_ids "
        f"{record['too_large_ids']}: {hwm} kB"
    )


def runs(runner: Path, test: Path, work: Path) -> bool:
    bed = Bed(runner.parent, work / "runs")
    bed.work.mkdir(mode=0o700)
    names = [f"b{n:03}" for n in range(1, RUNS + 1)]
    for name in names:
        directory = bed.connector(name, ["bin/ulei-connector-test"], every=EVERY)
        os.link(test, directory / "bin" / "ulei-connector-test")
    bed.write(names, {}, sync_every=EVERY)
    archive = FakeArchive(bed.c10)
    try:
        daemon, ready = spawn(runner, bed.config, SCOPE)
        said: list[bytes] = []
        threading.Thread(target=drain, args=(daemon, said), daemon=True).start()
        try:
            peaks = Peaks(daemon.pid)
            began = time.monotonic()
            while (done := len(run_records(bed.log))) < RUNS:
                alive(daemon, f"after {done} runs")
                if time.monotonic() - began > RUNS_TIMEOUT:
                    raise Failure(f"{done} of {RUNS} runs in {RUNS_TIMEOUT} s")
                peaks.sample()
                time.sleep(SAMPLE_SECONDS)
            took = time.monotonic() - began
            hwm = proc_field(daemon.pid, "status", "VmHWM")
            wchar = proc_field(daemon.pid, "io", "wchar")
            memory = int((peaks.group / "memory.peak").read_text(encoding="ascii")) // 1024
            daemon.send_signal(signal.SIGTERM)
            status = daemon.wait(timeout=60)
        finally:
            if daemon.poll() is None:
                stop(daemon)
        records = bed.records()
        puts = sum(len(archive.of(name)) for name in names)
    finally:
        archive.close()
    wrong = [r for r in records if r["outcome"] != "ok"]
    if status != 0 or len(records) != RUNS or wrong or puts != RUNS * ITEMS:
        tail = b"".join(said).decode(errors="replace").splitlines()[-3:]
        raise Failure(
            f"exit {status}, {len(records)} records, {puts} puts; not ok: "
            f"{[(r['connector'], r['outcome'], r.get('code')) for r in wrong[:3]]}; "
            f"stderr: {tail}"
        )
    per_run = wchar // RUNS
    mcp_hwm, mcp_text = mcp_page(runner, work / "mcp")
    scheme = (
        f"{RUNS} enabled test connectors, never run, so all due at start; serve ran them four at "
        f"a time, each one run of {ITEMS} items into a fake C10, in {took:.1f} s after its "
        f"ready line at {ready * 1e3:.1f} ms"
    )
    annotate("notice", f"{TITLE}: four runs at once", (
        f"{scheme}; sampled every {SAMPLE_SECONDS * 1e3:.0f} ms: at most {peaks.threads} "
        f"threads of serve, {peaks.children} children (systemd-run) and {peaks.tasks} tasks in "
        f"its scope {peaks.group.name}; memory.peak of the scope {memory} kB"
    ))
    passed = True
    for name, value, limit, text in (
        ("VmHWM after the runs", max(hwm, mcp_hwm), MAX_RSS_KB,
         f"VmHWM of serve after {RUNS} runs: {hwm} kB; {mcp_text} (budget {MAX_RSS_KB} kB "
         f"each)"),
        ("wchar per run", per_run, MAX_WRITE_BYTES,
         f"wchar of serve and its reaped children over {RUNS} runs: {wchar} B, {per_run} B per "
         f"run (budget {MAX_WRITE_BYTES} B)"),
    ):
        annotate("notice", f"{TITLE}: {name}", text)
        if value > limit:
            annotate("error", f"{TITLE}: {name}", f"{value} over the budget {limit}")
            passed = False
    return passed


def main(argv: list[str]) -> int:
    if len(argv) == 3 and argv[0] == "idle":
        try:
            results = idle(Path(argv[1]).resolve(), Path(argv[2]).resolve())
        except (Failure, OSError, KeyError, ValueError, subprocess.SubprocessError) as e:
            print(f"connectors_budget: {e}", file=sys.stderr)
            return 1
        for name, value in results.items():
            print(name, value)
        return 0
    if len(argv) == 4 and argv[0] == "runs":
        try:
            passed = runs(*(Path(a).resolve() for a in argv[1:]))
        except (Failure, OSError, KeyError, ValueError, subprocess.SubprocessError) as e:
            annotate("error", f"{TITLE}: runs", str(e))
            return 1
        return 0 if passed else 1
    print(
        "usage: connectors_budget.py idle ULEI_CONNECTORS WORKDIR\n"
        "       connectors_budget.py runs ULEI_CONNECTORS ULEI_CONNECTOR_TEST WORKDIR",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
