"""Driver of the `ulei-memory` budgets in ``ci/budgets.sh`` (ADR-0051 section 7).

``python3 ci/memory_run.py ULEI_MEMORY WORKDIR`` writes a synthetic ``ulei.toml`` with a fresh
archive under WORKDIR and a fake engine serving C6 `raw_sessions` from a synthetic dialogue,
whose rows the first start pulls; it fills the archive through C10 `put` with fictional notes,
restarts ``serve`` on the filled archive, drives it with `put`, `search` and `read`, and measures
its idle. Every start and `search` pulls again: the engine's last page carries no cursor, so each
pull reads the ``DIALOGUE_ROWS % PAGE_ROWS`` rows of the last page again. It prints one
``<name> <value>`` line per measurement. Any non-200 answer or unexpected body is a failure.
"""

from __future__ import annotations

import base64
import json
import socket
import sys
import threading
import time
from pathlib import Path

from garden_run import PEOPLE, READY_RESTARTS, alive, fact, median_ready, p50, query, spawn
from inbox_load import Failure, call, proc_field, read_message, stop, thread_switches

FILL_ITEMS = 10_000
REQUESTS = 10_000
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
SOURCE = "notes"
KIND = "text/plain; charset=utf-8"
FIRST_AT = 1_700_000_000
DIALOGUE_ROWS = 1_050
SKIP_EVERY = 105
PAGE_ROWS = 100
ENGINE_TIMEOUT = 5.0


def dialogue_row(n: int) -> tuple[str, dict]:
    """Journal row ``n`` (from 1) of the fake engine: ``("skipped", RawSkipped)`` for every
    ``SKIP_EVERY``-th row, else ``("messages", RawMessage)`` of one of ten sessions."""
    session = n % 10 + 1
    seq = n // 10
    head = {"session_id": str(session), "session_key": f"session-{session}", "seq": seq,
            "ts": FIRST_AT + n}
    if n % SKIP_EVERY == 0:
        return "skipped", {**head, "len": 600_000}
    role = "user" if seq % 2 == 0 else "assistant"
    text = f"{PEOPLE[n % len(PEOPLE)][0]} {fact(n)}."
    message = json.dumps({"role": role, "content": [{"type": "text", "text": text}]},
                         ensure_ascii=False, separators=(",", ":"))
    return "messages", {**head, "role": role, "content": text, "message": message}


class FakeEngine:
    """The engine's C6 `raw_sessions` on a Unix socket over ``DIALOGUE_ROWS`` journal rows: the
    rows after the cursor, at most the query's `limit`, skipped rows in `skipped`, and a cursor
    only when more follow."""

    def __init__(self, path: str) -> None:
        self.rows = [dialogue_row(n) for n in range(1, DIALOGUE_ROWS + 1)]
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(8)
        self.requests = 0
        self.errors: list[str] = []
        self.lock = threading.Lock()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            with conn:
                conn.settimeout(ENGINE_TIMEOUT)
                try:
                    self.answer(conn)
                except (Failure, OSError, ValueError, KeyError) as e:
                    with self.lock:
                        self.errors.append(f"fake engine: {e!r}")

    def answer(self, conn: socket.socket) -> None:
        head, body = read_message(conn, "raw_sessions")
        line = head.split(b"\r\n", 1)[0]
        if not line.startswith(b"POST /v1/raw_sessions "):
            raise Failure(f"fake engine got {line.decode(errors='replace')}")
        query = json.loads(body)
        after = int(query.get("cursor", "0"))
        rows = self.rows[after:after + query.get("limit", 10_000)]
        page: dict = {"messages": []}
        for kind, row in rows:
            page.setdefault(kind, []).append(row)
        if after + len(rows) < len(self.rows):
            page["cursor"] = str(after + len(rows))
        reply = json.dumps(page, ensure_ascii=False).encode()
        conn.sendall(
            f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(reply)}\r\nConnection: close\r\n\r\n".encode() + reply
        )
        with self.lock:
            self.requests += 1

    def check(self) -> None:
        with self.lock:
            if self.errors:
                raise Failure("; ".join(self.errors))

    def close(self) -> None:
        self.listener.close()


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


def write_config(root: Path, socket_path: str, engine_path: str) -> Path:
    """Writes the C9 document of the run under ``root`` and returns it."""
    config = root / "ulei.toml"
    config.write_text(
        f'[memory]\narchive = "{root / "archive" / "memory.archive"}"\n'
        f'socket = "{socket_path}"\n\n[engine]\nc6_socket = "{engine_path}"\n',
        encoding="utf-8",
    )
    return config


def pulled(path: str) -> tuple[int, int]:
    """The dialogue items and skipped references the daemon's start pulled, checked against the
    fake engine's rows."""
    health, _ = call(path, "health", {})
    if "sync_error" in health or "last_sync_at" not in health:
        raise Failure(f"the start pull failed: {health}")
    skipped = len(health.get("skipped", []))
    expected = DIALOGUE_ROWS // SKIP_EVERY
    if skipped != expected:
        raise Failure(f"health lists {skipped} skipped references, expected {expected}")
    items = health["items"] - FILL_ITEMS
    if items != DIALOGUE_ROWS - expected:
        raise Failure(f"the start pull stored {items} dialogue items")
    return items, skipped


def fill(memory: str, config: Path, path: str) -> tuple[list[str], int, tuple[int, int]]:
    """Fills the fresh archive with ``FILL_ITEMS`` notes through `put` after the start pulled
    the dialogue; returns their ids, the daemon's wchar per stored put and what the pull
    stored."""
    daemon, _ = spawn(memory, config, path)
    try:
        start = proc_field(daemon.pid, "io", "wchar")
        ids = [put(path, i)[0] for i in range(FILL_ITEMS)]
        end = proc_field(daemon.pid, "io", "wchar")
        alive(daemon, "during the fill")
        dialogue = pulled(path)
    finally:
        stop(daemon)
    if len(set(ids)) != FILL_ITEMS:
        raise Failure(f"the fill stored {len(set(ids))} items, expected {FILL_ITEMS}")
    return ids, (end - start) // FILL_ITEMS, dialogue


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
            if not answer["hits"] or not answer["synced"]:
                raise Failure(f"search {query(n)!r} answered {answer}")
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
    engine = FakeEngine(str(root / "engine.sock"))
    try:
        config = write_config(root, path, str(root / "engine.sock"))
        ids, written, (dialogue_items, dialogue_skipped) = fill(memory, config, path)
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
        engine.check()
        pulls = engine.requests
    finally:
        engine.close()
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
        "dialogue_items": dialogue_items,
        "dialogue_skipped": dialogue_skipped,
        "dialogue_reread": DIALOGUE_ROWS % PAGE_ROWS,
        "pull_requests": pulls,
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
