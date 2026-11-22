"""Driver of the `ulei-memory` budgets in ``ci/budgets.sh`` (ADR-0051 section 7).

``python3 ci/memory_run.py ULEI_MEMORY WORKDIR`` writes a synthetic ``ulei.toml`` with a fresh
archive under WORKDIR, fills it through C10 `put` with fictional notes, restarts ``serve`` on
the filled archive, drives it with `put`, `search` and `read`, and measures its idle. The
archive's dialogue pull (ADR-0051 section 8 card 9) does not exist yet, so the run serves no fake
C6 socket and no `raw_sessions`. It prints one ``<name> <value>`` line per measurement. Any
non-200 answer or unexpected body is a failure.
"""

from __future__ import annotations

import base64
import sys
import time
from pathlib import Path

from garden_run import PEOPLE, READY_RESTARTS, alive, fact, median_ready, p50, query, spawn
from inbox_load import Failure, call, proc_field, stop, thread_switches

FILL_ITEMS = 10_000
REQUESTS = 10_000
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
SOURCE = "notes"
KIND = "text/plain; charset=utf-8"
FIRST_AT = 1_700_000_000


def note(index: int) -> tuple[str, str, bytes]:
    """The author, indexed text and original bytes of note ``index``; the bytes are distinct for
    every index."""
    who = PEOPLE[index % len(PEOPLE)][0]
    text = f"{who} {fact(index)}."
    return who, text, f"Note {index:05d}. {text}\n".encode()


def put(path: str, index: int) -> tuple[str, float]:
    """Puts note ``index`` as a new entry; returns its item id and the round trip."""
    who, text, original = note(index)
    answer, elapsed = call(path, "put", {
        "source": SOURCE,
        "external_id": f"note-{index:05d}",
        "who": who,
        "at": FIRST_AT + index * 60,
        "kind": KIND,
        "text": text,
        "bytes_b64": base64.b64encode(original).decode("ascii"),
    })
    if answer.get("status") != "stored":
        raise Failure(f"put of note {index} answered {answer}")
    return answer["id"], elapsed


def write_config(root: Path, socket_path: str) -> Path:
    """Writes the C9 document of the run under ``root`` and returns it."""
    config = root / "ulei.toml"
    config.write_text(
        f'[memory]\narchive = "{root / "archive" / "memory.archive"}"\n'
        f'socket = "{socket_path}"\n',
        encoding="utf-8",
    )
    return config


def fill(memory: str, config: Path, path: str) -> tuple[list[str], int]:
    """Fills the fresh archive with ``FILL_ITEMS`` notes through `put`; returns their ids and
    the daemon's wchar per stored put."""
    daemon, _ = spawn(memory, config, path)
    try:
        start = proc_field(daemon.pid, "io", "wchar")
        ids = [put(path, i)[0] for i in range(FILL_ITEMS)]
        end = proc_field(daemon.pid, "io", "wchar")
        alive(daemon, "during the fill")
    finally:
        stop(daemon)
    if len(set(ids)) != FILL_ITEMS:
        raise Failure(f"the fill stored {len(set(ids))} items, expected {FILL_ITEMS}")
    return ids, (end - start) // FILL_ITEMS


def read(path: str, ids: list[str], index: int) -> float:
    """Reads note ``index`` of the fill and checks its original; returns the round trip."""
    answer, elapsed = call(path, "read", {"id": ids[index]})
    original = note(index)[2]
    if base64.b64decode(answer["bytes_b64"]) != original or answer["len"] != len(original):
        raise Failure(f"read of note {index} answered {answer['len']} other bytes")
    return elapsed


def load(path: str, ids: list[str]) -> dict[str, int]:
    """``REQUESTS`` requests in turn `put` of a new note, `search` of an activity and a place
    word that notes of the fill hold, and `read` of a note of the fill: p50 of each from connect
    to last byte."""
    times: dict[str, list[float]] = {"put": [], "search": [], "read": []}
    for i in range(REQUESTS):
        n = i // 3
        if i % 3 == 0:
            times["put"].append(put(path, FILL_ITEMS + n)[1])
        elif i % 3 == 1:
            answer, elapsed = call(path, "search", {"q": query(n)})
            if not answer["hits"]:
                raise Failure(f"search {query(n)!r} found nothing")
            times["search"].append(elapsed)
        else:
            times["read"].append(read(path, ids, n * 7919 % FILL_ITEMS))
    return {
        "put_p50_us": p50(times["put"]),
        "put_requests": len(times["put"]),
        "search_p50_us": p50(times["search"]),
        "search_requests": len(times["search"]),
        "read_p50_us": p50(times["read"]),
        "read_requests": len(times["read"]),
    }


def measure(memory: str, work: Path) -> dict[str, int]:
    root = work / "memory"
    root.mkdir(mode=0o700)
    path = str(root / "run" / "memory.sock")
    config = write_config(root, path)
    ids, written = fill(memory, config, path)
    ready = median_ready(memory, config, path)
    daemon, _ = spawn(memory, config, path)
    try:
        latencies = load(path, ids)
        alive(daemon, "during the load")
        time.sleep(SETTLE_SECONDS)
        before = thread_switches(daemon.pid)
        time.sleep(IDLE_SECONDS)
        after = thread_switches(daemon.pid)
        alive(daemon, "while idle")
        hwm = proc_field(daemon.pid, "status", "VmHWM")
    finally:
        stop(daemon)
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    if len(after) != 1:
        raise Failure(f"serve runs {len(after)} threads, expected one")
    return {
        "fill_items": FILL_ITEMS,
        "write_bytes_per_put": written,
        "ready_us": ready,
        "ready_restarts": READY_RESTARTS,
        "requests": REQUESTS,
        "vmhwm_kb": hwm,
        **latencies,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: memory_run.py ULEI_MEMORY WORKDIR", file=sys.stderr)
        return 2
    try:
        results = measure(argv[0], Path(argv[1]))
    except (Failure, OSError, KeyError, ValueError) as e:
        print(f"memory_run: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
