"""Source connectors end to end with the real archive under the installed runner unit (ADR-0056
section 9, card 12).

``python3 ci/connectors_e2e.py RELEASE ULEI_MEMORY WORKDIR`` needs a running user manager reachable
over the user bus with the release ``deploy/install.sh`` put in place, RELEASE the release
directory and ULEI_MEMORY the ``ulei-memory`` binary. WORKDIR must lie outside ``/tmp`` and
``/var/tmp``, which PrivateTmp hides from the unit. It installs the released test connector with
``deploy/install.sh --connector test --dir WORKDIR/connectors`` and copies it there under the names
of ``COPIES``, starts ``ulei-memory serve`` on a fresh archive whose C6 pull goes to the fake engine
of ``ci/memory_run.py``, writes a synthetic ``ulei.toml`` into the manager's config directory and
starts ``ulei-connectors.service``. It then checks that the scheduled run of ``test`` ends ``ok``
with its items found by C10 `search` with `source` ``test``, as many as the run stored; that the
unit's scheduled `sync` reaches the real daemon; that after ``reset`` a manual ``run test`` stores
nothing and finds every item a duplicate; that four copies due at start run at once inside the
unit's ``MemoryMax`` and ``TasksMax``; and that a stop leaves no run unit and no broker socket.

The passed checks fold into three notices, one line per check with its measured value; a failed
check is an error annotation of its own, and any failed check makes the exit status 1.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

from annotations import Notices, annotate
from connectors_run import Bed, run_records
from garden_run import alive, spawn
from inbox_load import Failure, call, stop
from memory_run import DIALOGUE_ROWS, SKIP_EVERY, FakeEngine, write_config
from units_run import (
    CONNECTORS, ITEMS, PRIVATE_TMP, POLL_SECONDS, RUN_TIMEOUT, host, journal, layout, poll, show,
    start, systemctl,
)

SOURCE = "test"
COPIES = ("test2", "test3", "test4", "test5")
SYNC_EVERY = 5
# Two scheduled syncs, the one at start and the next, within this many seconds of the start.
SYNC_TIMEOUT = 2 * SYNC_EVERY + 10
# C10 `search` answers at most 50 hits; a group of item numbers stays under it, so a group's
# hits are all its matches.
SEARCH_LIMIT = 50
GROUP = 25
SYNC_FAILURE = "ulei-connectors: sync:"
FOLDS = (
    "connectors end to end: scheduled run and sync",
    "connectors end to end: reset and repeated run",
    "connectors end to end: four runs at once and stop",
)


class EndToEnd:
    """The bed, the archive's socket and what the checks measured, in the order they run."""

    def __init__(self, bed: Bed, engine: FakeEngine) -> None:
        self.bed = bed
        self.engine = engine
        self.c10 = str(bed.c10)
        self.base: dict = {}
        self.pulls = 0
        self.started = 0.0
        self.first: dict = {}

    def health(self) -> dict:
        return call(self.c10, "health", {})[0]

    def found(self) -> tuple[set[str], int]:
        """The external ids of the items `search` with `source` ``test`` finds over the item
        numbers 1 to ``ITEMS + GROUP``, and the number of searches."""
        ids: set[str] = set()
        searches = 0
        for low in range(1, ITEMS + GROUP + 1, GROUP):
            q = " OR ".join(str(n) for n in range(low, low + GROUP))
            answer, _ = call(self.c10, "search", {"q": q, "source": SOURCE, "limit": SEARCH_LIMIT})
            searches += 1
            if not answer["synced"] or len(answer["hits"]) >= SEARCH_LIMIT:
                raise Failure(
                    f"search of numbers {low} to {low + GROUP - 1}: synced {answer['synced']}, "
                    f"{len(answer['hits'])} hits"
                )
            for hit in answer["hits"]:
                sources = {entry["source"] for entry in hit["entries"]}
                if sources != {SOURCE}:
                    raise Failure(f"hit {hit['id']} has entries of {sorted(sources)}")
                ids |= {entry["external_id"] for entry in hit["entries"]}
        return ids, searches

    def records(self, names: tuple[str, ...], since: float) -> list[dict]:
        return [
            r for r in run_records(self.bed.log) if r["connector"] in names and r["at"] >= since
        ]


def install(release: Path, bed: Bed) -> str:
    """The released test connector installed by name into ``bed.dir``, then copied there under
    each name of ``COPIES`` with its manifest renamed."""
    script = Path(__file__).resolve().parent.parent / "deploy" / "install.sh"
    done = subprocess.run(
        [str(script), "--connector", SOURCE, "--dir", str(bed.dir), str(release)],
        capture_output=True, text=True, check=False, timeout=60,
    )
    if done.returncode != 0:
        raise Failure(f"install.sh --connector {SOURCE}: exit {done.returncode}: {done.stderr}")
    installed = bed.dir / SOURCE
    manifest = json.loads((installed / "connector.json").read_text(encoding="utf-8"))
    for name in COPIES:
        shutil.copytree(installed, bed.dir / name)
        (bed.dir / name / "connector.json").write_text(
            json.dumps({**manifest, "name": name}), encoding="utf-8",
        )
    files = sorted(p.name for p in installed.iterdir())
    return (
        f"deploy/install.sh --connector {SOURCE} --dir WORKDIR/connectors: exit 0, "
        f"{SOURCE}/{{{','.join(files)}}}; copies {', '.join(COPIES)}"
    )


def scheduled(e2e: EndToEnd) -> str:
    """The unit's scheduled run of ``test`` ends ``ok``; `search` finds as many items as it
    stored, and `health` counts them on top of the pulled dialogue."""
    ran = poll(f"run record of {SOURCE}", RUN_TIMEOUT, lambda: e2e.records((SOURCE,), e2e.started))
    record = ran[0]
    if record["outcome"] != "ok" or record["stored"] != ITEMS:
        raise Failure(f"run record {record}")
    e2e.first = record
    ids, searches = e2e.found()
    items = e2e.health()["items"]
    wrong = []
    if ids != {f"item-{n}" for n in range(1, ITEMS + 1)}:
        wrong.append(f"search found {len(ids)} items of source {SOURCE}")
    if items != e2e.base["items"] + record["stored"]:
        wrong.append(f"health counts {items} items, {e2e.base['items']} before the run")
    if wrong:
        raise Failure(f"run stored {record['stored']}; " + "; ".join(wrong))
    return (
        f"unit {record['unit']}: outcome ok, stored {record['stored']}, duplicate "
        f"{record['duplicate']}, landlock_abi {record['landlock_abi']}, {record['duration_ms']} "
        f"ms; {searches} C10 searches with source {SOURCE} found {len(ids)} items "
        f"item-1 to item-{ITEMS}; health items {e2e.base['items']} (pulled dialogue) + "
        f"{record['stored']} = {items}"
    )


def synced(e2e: EndToEnd) -> str:
    """The unit's `sync` at start and the next one reach the real daemon, with no search in
    between to pull for it, and serve logs no sync failure."""
    seen: list[float] = []
    deadline = time.monotonic() + SYNC_TIMEOUT
    while len(seen) < 2:
        at = e2e.health().get("last_sync_at")
        if at is not None and at > e2e.started and at not in seen:
            seen.append(at)
        if time.monotonic() > deadline:
            raise Failure(
                f"health last_sync_at after the unit's start: {seen} within {SYNC_TIMEOUT} s"
            )
        time.sleep(POLL_SECONDS)
    e2e.engine.check()
    failed = journal(int(e2e.started), SYNC_FAILURE)
    if failed:
        raise Failure(f"sync failure lines: {failed}")
    offsets = [round(at - e2e.started, 1) for at in seen]
    return (
        f"sync_every {SYNC_EVERY}: health last_sync_at {offsets} s after the unit's start, "
        f"{e2e.base['last_sync_at'] - e2e.started:.1f} s at the daemon's own start pull; fake "
        f"engine raw_sessions requests {e2e.pulls} at the unit's start, "
        f"{e2e.engine.requests} after; no sync failure line in the unit's journal"
    )


def repeated(e2e: EndToEnd) -> str:
    """After ``reset``, a manual ``run test`` delivers the same items, all duplicates."""
    if not e2e.first:
        raise Failure("no first run to repeat")
    before = e2e.health()["items"]
    reset = subprocess.run(
        [str(e2e.bed.runner), "reset", SOURCE, "--config", str(e2e.bed.config)],
        capture_output=True, text=True, check=False, timeout=30,
    )
    if reset.returncode != 0:
        raise Failure(f"reset {SOURCE}: exit {reset.returncode}: {reset.stderr.strip()}")
    done, record = e2e.bed.run(SOURCE)
    ids, _ = e2e.found()
    after = e2e.health()["items"]
    stored = e2e.first["stored"]
    if (
        done.returncode != 0 or record["outcome"] != "ok" or record["stored"] != 0
        or record["duplicate"] != stored or after != before or len(ids) != stored
    ):
        raise Failure(
            f"exit {done.returncode}, record outcome {record['outcome']} stored "
            f"{record['stored']} duplicate {record['duplicate']}, first stored {stored}; health "
            f"items {before} -> {after}; search finds {len(ids)}: {done.stderr.strip()}"
        )
    return (
        f"{reset.stderr.strip()}; manual run test: exit 0, unit {record['unit']}, outcome ok, "
        f"stored 0, duplicate {record['duplicate']} (first run stored {stored}); health items "
        f"{before} -> {after}; search still finds {len(ids)}"
    )


def peaks(cgroup: Path) -> tuple[int, int]:
    """``memory.peak`` and ``pids.peak`` of ``cgroup``."""
    memory, tasks = (
        int((cgroup / name).read_text(encoding="ascii")) for name in ("memory.peak", "pids.peak")
    )
    return memory, tasks


def overlap(records: list[dict]) -> int:
    """The most runs of ``records`` in flight at one time."""
    edges = sorted(
        (t, step) for r in records
        for t, step in ((r["at"], 1), (r["at"] + r["duration_ms"] / 1000, -1))
    )
    most = current = 0
    for _, step in edges:
        current += step
        most = max(most, current)
    return most


def four(e2e: EndToEnd) -> str:
    """The copies, never run and so all due at start, run at once in the restarted unit, whose
    cgroup peaks stay within its MemoryMax and TasksMax."""
    e2e.bed.write([SOURCE, *COPIES], {}, sync_every=SYNC_EVERY)
    since = time.time()
    done = systemctl("restart", CONNECTORS)
    if done.returncode != 0:
        raise Failure(f"systemctl --user restart {CONNECTORS}: {done.stderr.strip()}")
    unit = show(CONNECTORS, "ControlGroup", "MemoryMax", "TasksMax", "MainPID")
    cgroup = Path("/sys/fs/cgroup" + unit["ControlGroup"])
    active: list[str] = []
    during: tuple[int, int] | None = None
    deadline = time.monotonic() + RUN_TIMEOUT
    while len(ran := e2e.records(COPIES, since)) < len(COPIES):
        if during is None:
            active = systemctl(
                "list-units", "--plain", "--no-legend", "--state=active",
                *(f"ulei-connector-{name}-*.service" for name in COPIES),
            ).stdout.splitlines()
            if active:
                during = peaks(cgroup)
        if time.monotonic() > deadline:
            raise Failure(f"{len(ran)} run records of the copies within {RUN_TIMEOUT} s")
        time.sleep(POLL_SECONDS)
    memory, tasks = peaks(cgroup)
    left = sorted(p.name for p in e2e.bed.run_dir.iterdir())
    at_once = overlap(ran)
    memory_max, tasks_max = int(unit["MemoryMax"]), int(unit["TasksMax"])
    wrong = []
    if {r["outcome"] for r in ran} != {"ok"}:
        wrong.append(f"outcomes {[(r['connector'], r['outcome']) for r in ran]}")
    if at_once != len(COPIES):
        wrong.append(f"at most {at_once} runs in flight at once")
    if memory > memory_max or tasks > tasks_max:
        wrong.append(f"memory.peak {memory} of {memory_max}, pids.peak {tasks} of {tasks_max}")
    if left:
        wrong.append(f"left in run_dir: {left}")
    if wrong:
        raise Failure("; ".join(wrong))
    looked = (
        f"with {len(active)} run units active: memory.peak {during[0]} B, pids.peak {during[1]}"
        if during is not None else "the runs ended before a look found one active"
    )
    return (
        f"{len(COPIES)} copies due at start in pid {unit['MainPID']}: {at_once} runs in flight at "
        f"once by their records' at and duration_ms, all ok, stored "
        f"{[r['stored'] for r in ran]}; {cgroup.name} {looked}; after the runs memory.peak "
        f"{memory} B (MemoryMax {memory_max} B), pids.peak {tasks} (TasksMax {tasks_max}); "
        "run_dir empty"
    )


def stopped(e2e: EndToEnd) -> str:
    """A stop of the unit leaves no run unit active and no broker socket in ``run_dir``."""
    done = systemctl("stop", CONNECTORS)
    if done.returncode != 0:
        raise Failure(f"systemctl --user stop {CONNECTORS}: {done.stderr.strip()}")
    state = show(CONNECTORS, "ActiveState", "Result")
    if state != {"ActiveState": "inactive", "Result": "success"}:
        raise Failure(f"{CONNECTORS} after stop: {state}")
    units = systemctl(
        "list-units", "--plain", "--no-legend", "--state=active", "ulei-connector-*.service",
    ).stdout.split()
    run_dir = e2e.bed.run_dir
    left = sorted(p.name for p in run_dir.iterdir()) if run_dir.exists() else []
    failed = journal(int(e2e.started), SYNC_FAILURE)
    if units or left or failed:
        raise Failure(f"active run units {units}; left in run_dir {left}; sync failures {failed}")
    records = run_records(e2e.bed.log)
    return (
        f"inactive with result success; no ulei-connector-*.service active; run_dir "
        f"{'empty' if run_dir.exists() else 'removed'}; {len(records)} run records, all "
        f"{sorted({r['outcome'] for r in records})}; no sync failure line over the whole run"
    )


def run(release: Path, memory: str, work: Path) -> bool:
    notices = Notices()
    try:
        return run_checks(notices, release, memory, work)
    finally:
        notices.write()


def run_checks(notices: Notices, release: Path, memory: str, work: Path) -> bool:
    dirs = layout()
    hidden = [d for d in PRIVATE_TMP if work.is_relative_to(d)]
    if hidden:
        raise Failure(f"WORKDIR {work} is under {hidden[0]}, which PrivateTmp hides from the unit")
    if dirs["state"].exists():
        shutil.rmtree(dirs["state"])
    dirs["config"].mkdir(parents=True, exist_ok=True)
    bed = Bed(dirs["bin"], work)
    bed.state = dirs["state"] / "connectors" / "state"
    bed.log = dirs["state"] / "connectors" / "runs.jsonl"
    bed.run_dir = dirs["connectors_run"]
    bed.config = dirs["config"] / "ulei.toml"
    root = work / "memory"
    root.mkdir(mode=0o700)
    bed.c10 = root / "run" / "memory.sock"
    engine_socket = str(root / "engine.sock")
    engine = FakeEngine(engine_socket)
    run_first, run_again, run_four = FOLDS
    try:
        if not notices.check(run_first, "connectors e2e install", lambda: install(release, bed)):
            return False
        daemon, _ = spawn(memory, write_config(root, str(bed.c10), engine_socket), str(bed.c10))
        try:
            e2e = EndToEnd(bed, engine)
            e2e.base = e2e.health()
            expected = DIALOGUE_ROWS - DIALOGUE_ROWS // SKIP_EVERY
            if e2e.base["items"] != expected or "last_sync_at" not in e2e.base:
                raise Failure(f"the daemon's start pull: health {e2e.base}, {expected} items due")
            bed.write([SOURCE], {}, sync_every=SYNC_EVERY)
            e2e.pulls = engine.requests
            e2e.started = time.time()
            done = systemctl("start", CONNECTORS)
            print(f"systemctl --user start: exit {done.returncode} {done.stderr.strip()}")
            if not notices.check(
                run_first, f"connectors e2e start {CONNECTORS}", lambda: start(CONNECTORS, host()),
            ):
                return False
            # Every `search` pulls, so the sync check runs before the first one.
            passed = notices.check(run_first, "connectors e2e sync", lambda: synced(e2e))
            passed &= notices.check(
                run_first, "connectors e2e scheduled run in the archive", lambda: scheduled(e2e),
            )
            passed &= notices.check(
                run_again, "connectors e2e reset and repeated run", lambda: repeated(e2e),
            )
            passed &= notices.check(run_four, "connectors e2e four runs at once", lambda: four(e2e))
            passed &= notices.check(run_four, "connectors e2e stop", lambda: stopped(e2e))
            alive(daemon, "during the checks")
            return passed
        finally:
            stop(daemon)
    finally:
        systemctl("stop", CONNECTORS)
        systemctl("stop", "ulei-connector-*.service")
        engine.close()


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print("usage: connectors_e2e.py RELEASE ULEI_MEMORY WORKDIR", file=sys.stderr)
        return 2
    try:
        passed = run(Path(argv[0]).resolve(), argv[1], Path(argv[2]).resolve())
    except (Failure, OSError, KeyError, subprocess.SubprocessError) as e:
        annotate("error", "connectors end to end", f"setup failed: {e}")
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
