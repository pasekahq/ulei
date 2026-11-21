"""Driver of the `ulei-garden` budgets in ``ci/budgets.sh`` (ADR-0047 section 7).

``python3 ci/garden_run.py ULEI_GARDEN WORKDIR`` writes a synthetic ``ulei.toml`` with the
embedder off, a producer key and a fresh store under WORKDIR, serves a fake C1 inbox there,
fills the store through C7 `propose` with fictional facts, restarts ``serve`` on the filled
store, drives it with `pointers`, `search` and `propose` and measures its idle. It prints one
``<name> <value>`` line per measurement. Any non-200 answer or unexpected body is a failure.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from inbox_load import (
    Failure,
    call,
    proc_field,
    read_message,
    stop,
    thread_switches,
)

FILL_FACTS = 2_000
REQUESTS = 10_000
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY_RESTARTS = 21
READY_TIMEOUT = 5.0
PUBLISH_TIMEOUT = 30.0
INBOX_TIMEOUT = 5.0
SOURCE_REF = "synthetic budget material"

# Transliterations of the full names are pairwise at least three edits apart, so a one-letter
# typo of the Latin spelling names exactly one person.
FIRST_NAMES = [
    ("Ведмира", "Vedmira"), ("Ольгерд", "Olgerd"), ("Миролюб", "Miroljub"),
    ("Златина", "Zlatina"), ("Борислав", "Borislav"), ("Радмила", "Radmila"),
    ("Всеволод", "Vsevolod"), ("Любава", "Ljubava"), ("Святомир", "Svjatomir"),
    ("Дарина", "Darina"),
]
SURNAMES = [
    ("Ольхастов", "Olhastov"), ("Брезняков", "Breznjakov"), ("Тумановец", "Tumanovec"),
    ("Кречетин", "Krecetin"), ("Сабуровский", "Saburovskij"), ("Ракитянский", "Rakitjanskij"),
    ("Жемчугов", "Zemcugov"), ("Велесов", "Velesov"), ("Гордеевич", "Gordeevic"),
    ("Ладожин", "Ladozin"), ("Мятлев", "Mjatlev"), ("Черноус", "Cernous"),
    ("Шиповник", "Sipovnik"), ("Ясколов", "Jaskolov"), ("Дубравин", "Dubravin"),
    ("Колыванов", "Kolyvanov"), ("Ферзиков", "Ferzikov"), ("Хмелёв", "Hmelev"),
    ("Цветаев", "Cvetaev"), ("Юрганов", "Jurganov"),
]
PEOPLE = [(f"{f} {s}", f"{fl} {sl}") for f, fl in FIRST_NAMES for s, sl in SURNAMES]

# (phrase, query word) pairs; a fact is one person with one of each list.
ACTIVITIES = [
    ("keeps a blue bicycle", "bicycle"), ("plays the cello", "cello"),
    ("buys sourdough bread", "sourdough"), ("walks a grey terrier", "terrier"),
    ("repairs old lanterns", "lanterns"), ("collects river pebbles", "pebbles"),
    ("sketches heron nests", "heron"), ("rents a rowing boat", "rowing"),
    ("sings in the choir", "choir"), ("grows tomato seedlings", "seedlings"),
    ("trades chess puzzles", "chess"), ("bakes plum tarts", "tarts"),
    ("borrows atlas volumes", "atlas"), ("feeds stray pigeons", "pigeons"),
    ("weaves wool scarves", "scarves"), ("tunes harpsichords", "harpsichords"),
    ("paints ceramic tiles", "ceramic"), ("brews juniper tea", "juniper"),
    ("binds leather journals", "journals"), ("photographs lighthouses", "lighthouses"),
]
PLACES = [
    ("near the Larkwell ferry", "Larkwell"), ("at the Brindlemoor market", "Brindlemoor"),
    ("in Ostvale gardens", "Ostvale"), ("by Quillhaven pier", "Quillhaven"),
    ("behind the Marrowfen library", "Marrowfen"), ("at Thistlecombe station", "Thistlecombe"),
    ("on Duskmere hill", "Duskmere"), ("inside the Fernholt arcade", "Fernholt"),
    ("along Cobblereach canal", "Cobblereach"), ("at the Wrenford bakery", "Wrenford"),
    ("near Velmora fountain", "Velmora"), ("in Tarnwick square", "Tarnwick"),
    ("by the Oskholm mill", "Oskholm"), ("at Pellbrook clinic", "Pellbrook"),
    ("under Hollowmere bridge", "Hollowmere"), ("in the Saltreach yard", "Saltreach"),
    ("next to Gorsefield school", "Gorsefield"), ("at the Ambervale theatre", "Ambervale"),
    ("around Kestrelby harbour", "Kestrelby"), ("beside Mirelock orchard", "Mirelock"),
]
TIMES = [
    "on rainy evenings", "every spring", "after the harvest fair", "before breakfast",
    "on long weekends", "during the winter thaw", "after choir practice", "in late autumn",
    "on market days", "at dusk", "when the tide is low", "before the first frost",
    "on quiet mornings", "during school holidays", "after the night shift",
    "in the dry season", "on feast days", "when the ferry is late", "after the storms",
    "at the turn of the year",
]
# Every tenth search asks one of these; each publishes one miss, its repeats coalesce.
MISS_EVERY = 10
MISSES = [
    f"{a} {b}"
    for a in ("zorbulent", "quaxifer", "drimbleth", "vorpaxine", "skelluvet")
    for b in ("glimtrove", "pondraxel", "frunsible", "wexhollow")
]


class FakeInbox:
    """A C1 inbox on a Unix socket that answers each `POST /v1/publish` of a `garden_miss`
    with `200` and the packet's id, and counts them."""

    def __init__(self, path: str) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(8)
        self.published = 0
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
                conn.settimeout(INBOX_TIMEOUT)
                try:
                    self.answer(conn)
                except (Failure, OSError, ValueError, KeyError) as e:
                    with self.lock:
                        self.errors.append(f"fake inbox: {e!r}")

    def answer(self, conn: socket.socket) -> None:
        head, body = read_message(conn, "publish")
        line = head.split(b"\r\n", 1)[0]
        if not line.startswith(b"POST /v1/publish "):
            raise Failure(f"fake inbox got {line.decode(errors='replace')}")
        packet = json.loads(body)["packet"]
        if packet["kind"] != "garden_miss":
            raise Failure(f"fake inbox got a packet of kind {packet['kind']}")
        reply = json.dumps({"id": packet["id"]}).encode()
        conn.sendall(
            f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(reply)}\r\nConnection: close\r\n\r\n".encode() + reply
        )
        with self.lock:
            self.published += 1

    def check(self) -> None:
        with self.lock:
            if self.errors:
                raise Failure("; ".join(self.errors))

    def close(self) -> None:
        self.listener.close()


def write_config(root: Path, socket_path: str, inbox_path: str) -> Path:
    """Writes the C9 document of the run under ``root`` with an owner-only synthetic producer
    key and returns it."""
    key = root / "producer.key"
    fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(fd, bytes(range(32)))
    finally:
        os.close(fd)
    config = root / "ulei.toml"
    config.write_text(
        f'[garden]\nstore = "{root / "store" / "garden.sqlite3"}"\nsocket = "{socket_path}"\n'
        f'producer_key_file = "{key}"\n\n[garden.embedder]\nenabled = false\n\n'
        f'[inbox]\nsocket = "{inbox_path}"\n',
        encoding="utf-8",
    )
    return config


def spawn(garden: str, config: Path, path: str) -> tuple[subprocess.Popen, float]:
    """A running ``serve`` of ``config`` on the socket ``path`` and the seconds from spawn to
    the first 200 of `health`."""
    spawned = time.perf_counter()
    daemon = subprocess.Popen(
        [garden, "serve", "--config", str(config)], stdout=subprocess.DEVNULL
    )
    try:
        while True:
            if daemon.poll() is not None:
                raise Failure(f"daemon exited with status {daemon.returncode} before ready")
            try:
                call(path, "health", {})
            except (FileNotFoundError, ConnectionRefusedError):
                if time.perf_counter() - spawned > READY_TIMEOUT:
                    raise Failure(f"no health answer within {READY_TIMEOUT} s") from None
                time.sleep(0.0002)
                continue
            return daemon, time.perf_counter() - spawned
    except BaseException:
        stop(daemon)
        raise


def alive(daemon: subprocess.Popen, during: str) -> None:
    if daemon.poll() is not None:
        raise Failure(f"daemon exited with status {daemon.returncode} {during}")


def fact(index: int) -> str:
    """The text of fact ``index``; distinct for every index below 8,000."""
    activity = ACTIVITIES[index % len(ACTIVITIES)][0]
    place = PLACES[index // len(ACTIVITIES) % len(PLACES)][0]
    when = TIMES[index // (len(ACTIVITIES) * len(PLACES)) % len(TIMES)]
    return f"{activity} {place} {when}"


def propose(path: str, entity: str, index: int) -> float:
    """Proposes fact ``index`` about the person ``entity``; returns the round trip of an
    accepted proposal."""
    answer, elapsed = call(path, "propose", {
        "fact": f"{entity} {fact(index)}.",
        "entity": entity,
        "kind": "person",
        "source_kind": "owner",
        "source_ref": SOURCE_REF,
    })
    if answer.get("status") != "accepted":
        raise Failure(f"propose of fact {index} about {entity} answered {answer}")
    return elapsed


def fill(garden: str, config: Path, path: str) -> int:
    """Fills the fresh store with ``FILL_FACTS`` accepted facts in Cyrillic spelling; returns
    the daemon's wchar per accepted propose."""
    daemon, _ = spawn(garden, config, path)
    try:
        wchar = proc_field(daemon.pid, "io", "wchar")
        for i in range(FILL_FACTS):
            propose(path, PEOPLE[i % len(PEOPLE)][0], i)
        wchar = proc_field(daemon.pid, "io", "wchar") - wchar
        alive(daemon, "during the fill")
    finally:
        stop(daemon)
    return wchar // FILL_FACTS


def median_ready(garden: str, config: Path, path: str) -> int:
    """Median microseconds from spawn to ready over ``READY_RESTARTS`` starts."""
    samples = []
    for _ in range(READY_RESTARTS):
        daemon, ready = spawn(garden, config, path)
        stop(daemon)
        samples.append(ready)
    return p50(samples)


def p50(samples: list[float]) -> int:
    return int(sorted(samples)[len(samples) // 2] * 1e6)


def query(n: int) -> str:
    """A search text that facts of the fill match: an activity word and a place word."""
    return f"{ACTIVITIES[n % len(ACTIVITIES)][1]} {PLACES[n // len(ACTIVITIES) % len(PLACES)][1]}"


def typo(latin: str) -> str:
    """``latin`` with its last letter replaced: one edit from its own transliteration."""
    return latin[:-1] + "x"


def load(path: str) -> dict[str, int]:
    """``REQUESTS`` requests in turn `pointers`, `search` and `propose`: p50 of each from
    connect to last byte and the pointers answered. A tenth of the searches ask a query of
    ``MISSES``; the proposes name a person by Latin spelling or with one typo."""
    times: dict[str, list[float]] = {"pointers": [], "search": [], "propose": []}
    pointed = 0
    for i in range(REQUESTS):
        n = i // 3
        if i % 3 == 0:
            answer, elapsed = call(path, "pointers", {"q": query(n)})
            pointed += bool(answer["pointers"])
            times["pointers"].append(elapsed)
        elif i % 3 == 1:
            miss = n % MISS_EVERY == 0
            q = MISSES[n // MISS_EVERY % len(MISSES)] if miss else query(n)
            answer, elapsed = call(path, "search", {"q": q})
            if bool(answer["facts"]) == miss:
                raise Failure(f"search {q!r} answered {len(answer['facts'])} facts")
            times["search"].append(elapsed)
        else:
            latin = PEOPLE[n % len(PEOPLE)][1]
            entity = latin if n % 2 == 0 else typo(latin)
            times["propose"].append(propose(path, entity, FILL_FACTS + n))
    return {
        "pointers_p50_us": p50(times["pointers"]),
        "pointers_answered": pointed,
        "pointers_requests": len(times["pointers"]),
        "search_p50_us": p50(times["search"]),
        "propose_p50_us": p50(times["propose"]),
    }


def published(path: str, inbox: FakeInbox) -> int:
    """Waits until `health` reports no unpublished miss; the publishes the fake inbox took."""
    deadline = time.perf_counter() + PUBLISH_TIMEOUT
    while True:
        health, _ = call(path, "health", {})
        if health["unpublished_misses"] == 0:
            break
        if time.perf_counter() > deadline:
            raise Failure(f"{health['unpublished_misses']} misses unpublished after "
                          f"{PUBLISH_TIMEOUT} s")
        time.sleep(0.05)
    inbox.check()
    if inbox.published != len(MISSES):
        raise Failure(f"fake inbox took {inbox.published} misses, expected {len(MISSES)}")
    return inbox.published


def measure(garden: str, work: Path) -> dict[str, int | str]:
    root = work / "garden"
    root.mkdir(mode=0o700)
    path = str(root / "run" / "garden.sock")
    inbox = FakeInbox(str(root / "c1.sock"))
    try:
        config = write_config(root, path, str(root / "c1.sock"))
        write_bytes = fill(garden, config, path)
        ready = median_ready(garden, config, path)
        daemon, _ = spawn(garden, config, path)
        try:
            latencies = load(path)
            alive(daemon, "during the load")
            misses = published(path, inbox)
            time.sleep(SETTLE_SECONDS)
            before = thread_switches(daemon.pid)
            time.sleep(IDLE_SECONDS)
            after = thread_switches(daemon.pid)
            alive(daemon, "while idle")
            hwm = proc_field(daemon.pid, "status", "VmHWM")
        finally:
            stop(daemon)
    finally:
        inbox.close()
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    if len(after) != 2:
        raise Failure(f"serve runs {len(after)} threads, expected the server and the worker")
    return {
        "fill_facts": FILL_FACTS,
        "write_bytes_per_propose": write_bytes,
        "ready_us": ready,
        "ready_restarts": READY_RESTARTS,
        "requests": REQUESTS,
        "vmhwm_kb": hwm,
        **latencies,
        "published_misses": misses,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: garden_run.py ULEI_GARDEN WORKDIR", file=sys.stderr)
        return 2
    try:
        results = measure(argv[0], Path(argv[1]))
    except (Failure, OSError, KeyError, ValueError) as e:
        print(f"garden_run: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
