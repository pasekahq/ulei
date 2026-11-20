"""Delivery kill test of the owner inbox (ADR-0037 section 6).

``python3 ci/deliver_kill.py ULEI_INBOX ULEI_ENGINE WORKDIR --events N --kills K --seed S`` runs
``ULEI_INBOX serve --deliver`` and ``ULEI_ENGINE serve`` on one test root under WORKDIR with a
fake gateway, a fake channel and a C6 proxy between consumer and engine, kills them with SIGKILL
at the points K1 to K9 and K times at random, reconciles every event against its C1 row and the
journal, prints one ``<name> <value>`` line per figure and the summary as one ``::notice`` line,
and exits 1 on a failed pass rule.
"""

from __future__ import annotations

import argparse
import json
import os
import random
import select
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from collections import Counter
from collections.abc import Callable
from contextlib import closing
from pathlib import Path

from engine_idle import SETTING_ENVS, read_turn, ready, serve_config, write_config
from inbox_load import Failure, call, proc_field, read_message, sign, spawn, stop

OWNER = "kill-owner"
TICK_MS = "50"
# `LEASE_TTL` and `RETRY_DELAY` of crates/core/src/inbox.rs.
LEASE_TTL = 120.0
RETRY_DELAY = 15.0
DRAIN_SECONDS = LEASE_TTL + RETRY_DELAY + 30.0
# Events per second of the random phase, below what the consumer settles at a 50 ms tick, and
# the unsettled events at which publishing waits, so the drain finds no backlog.
RATE = 3.0
MAX_UNSETTLED = 32
FIRE_SECONDS = 30.0
POLL_SECONDS = 0.1
RETRY_SECONDS = 0.05
PUBLISH_SECONDS = 60.0
MAX_PAUSE = 0.5
MAX_DELAY = 0.1
REFRESH_EVERY = 50
CONSUMER_SHARE = 0.45
ENGINE_SHARE = 0.45
PROMPT_USAGE = 1000
HEADER = "[INTERNAL BACKGROUND EVENT"
DONE = "NO_REPLY_DONE"
ANSWER = "Recorded the kill test event."
REFRESH = "Kill test session refresh."
SETTLED = ("done", "uncertain")
PHASES = ("claimed", "accepted", "in flight", "delivered", "turn running", "turn ended")
# Point: the process killed, the row status and the event's turns the forced kill ends in.
POINTS = {
    "K1": ("consumer", "done", 1),
    "K2": ("consumer", "done", 1),
    "K3": ("engine", "uncertain", 0),
    "K4": ("engine", "uncertain", 1),
    "K5": ("consumer", "uncertain", 1),
    "K6": ("consumer", "uncertain", 1),
    "K7": ("engine", "uncertain", 0),
    "K8": ("engine", "uncertain", 1),
    "K9": ("engine", "uncertain", 1),
}
STREAM_HEAD = (
    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n"
    b"Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
)


def event_id(n: int) -> str:
    return f"kill:{n:05d}"


def tail_of(text: str) -> dict | None:
    """The JSON tail of an event prompt, or None for any other text."""
    if not text.startswith(HEADER):
        return None
    for line in text.split("\n"):
        if line.startswith('{"event_id": '):
            return json.loads(line)
    raise Failure("an event prompt without its JSON tail")


def text_of(message: dict) -> str:
    return "".join(block["text"] for block in message["content"] if block["type"] == "text")


def turn_answer(text: str) -> bytes:
    """A gateway turn stream that answers ``text`` and ends the turn."""
    events = [
        {"type": "route", "route": {"vendor": "vendor-a", "model": "model-a",
                                    "login_label": "login-a", "lease": "lease-1"}},
        {"type": "text_delta", "text": text},
        {"type": "usage", "usage": {"input": PROMPT_USAGE, "output": 3, "cache_read": 0,
                                    "cache_write": 0}},
        {"type": "stop", "reason": "end"},
    ]
    out = STREAM_HEAD
    for event in events:
        line = json.dumps(event).encode() + b"\n"
        out += f"{len(line):x}\r\n".encode() + line + b"\r\n"
    return out + b"0\r\n\r\n"


def delivery_of(outcome: str, final: str | None) -> str:
    """The ADR-0036 section 6 classification of an ended inbox turn whose send succeeds."""
    if outcome not in ("completed", "halted") or not final or not final.strip():
        return "no_answer"
    return "silent_done" if final.strip() == DONE else "sent"


class Ledger:
    """Publish and settle times of the events; settled rows are final."""

    def __init__(self) -> None:
        self.changed = threading.Condition()
        self.published: dict[str, float] = {}
        self.settled: dict[str, tuple[dict, float]] = {}

    def publish(self, eid: str, at: float) -> None:
        with self.changed:
            self.published[eid] = at

    def unsettled(self) -> list[str]:
        with self.changed:
            return [eid for eid in self.published if eid not in self.settled]

    def settle(self, eid: str, row: dict, at: float) -> None:
        with self.changed:
            self.settled[eid] = (row, at)
            self.changed.notify_all()

    def wait(self, done: Callable[[], bool], seconds: float) -> bool:
        with self.changed:
            return self.changed.wait_for(done, seconds)


class Proxy:
    """The C6 proxy on ``proxy.sock``: relays each consumer connection to the engine and lets the
    harness act on what passes. It listens only while the engine runs."""

    def __init__(self, harness: Harness) -> None:
        self.harness = harness
        self.listener: socket.socket | None = None
        self.wake = (-1, -1)
        self.thread: threading.Thread | None = None

    def bind(self) -> None:
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(self.harness.proxy_sock)
        listener.listen(64)
        self.wake = os.pipe()
        self.listener = listener
        self.thread = threading.Thread(target=self.accept, args=(listener, self.wake[0]),
                                       daemon=True)
        self.thread.start()

    def unbind(self) -> None:
        if self.listener is None or self.thread is None:
            return
        os.write(self.wake[1], b"\0")
        self.thread.join()
        self.listener.close()
        os.close(self.wake[0])
        os.close(self.wake[1])
        os.unlink(self.harness.proxy_sock)
        self.listener = None

    def accept(self, listener: socket.socket, wake: int) -> None:
        while True:
            readable, _, _ = select.select([listener, wake], [], [])
            if wake in readable:
                return
            client, _ = listener.accept()
            threading.Thread(target=self.relay, args=(client,), daemon=True).start()

    def relay(self, client: socket.socket) -> None:
        harness = self.harness
        with client:
            try:
                head, body = read_message(client, "proxied request")
            except (Failure, OSError):
                return
            method = head.split(b" ", 2)[1].decode().removeprefix("/v1/")
            if method == "inject":
                harness.seen("claimed")
                if harness.fire("K1"):
                    return
            elif method == "status":
                harness.seen("in flight")
                if harness.fire("K5"):
                    return
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as engine:
                    engine.settimeout(FIRE_SECONDS)
                    engine.connect(harness.engine_sock)
                    engine.sendall(head + b"\r\n\r\n" + body)
                    answer_head, answer_body = read_message(engine, method)
            except (Failure, OSError):
                return
            if answer_head.startswith(b"HTTP/1.1 200 ") and self.withheld(method, answer_body):
                return
            try:
                client.sendall(answer_head + b"\r\n\r\n" + answer_body)
            except OSError:
                return

    def withheld(self, method: str, body: bytes) -> bool:
        """Acts on an engine answer; true when it is not relayed because a kill took its place."""
        harness = self.harness
        answer = json.loads(body)
        if method == "inject" and answer["accepted"]:
            harness.seen("accepted")
            if harness.fire("K2"):
                return True
            if harness.armed("K3"):
                harness.gateway_held.wait(FIRE_SECONDS)
                return harness.fire("K3")
            return harness.armed("K4") and harness.wait_fired()
        if method == "status" and (answer.get("turn") or {}).get("delivery") is not None:
            harness.seen("delivered")
            if harness.fire("K6"):
                return True
            return harness.armed("K9") and harness.wait_fired()
        return False


class Gateway:
    """The fake C4G gateway on ``qb.socket``: answers each turn with one assistant text after a
    uniform 0 to 100 ms delay, ``NO_REPLY_DONE`` for an event whose payload asks for it."""

    def __init__(self, harness: Harness, path: Path, rng: random.Random) -> None:
        self.harness = harness
        self.rng = rng
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(64)
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self) -> None:
        while True:
            conn, _ = self.listener.accept()
            threading.Thread(target=self.answer, args=(conn,), daemon=True).start()

    def answer(self, conn: socket.socket) -> None:
        harness = self.harness
        with conn:
            try:
                request = read_turn(conn)
            except (Failure, OSError):
                harness.count("gateway_cut")
                return
            users = [m for m in request["messages"] if m["role"] == "user"]
            tail = tail_of(text_of(users[-1])) if users else None
            if tail is not None:
                harness.seen("turn running")
                if harness.fire("K7"):
                    return
                if harness.armed("K3"):
                    harness.gateway_held.set()
                    harness.wait_fired()
                    return
            time.sleep(self.rng.uniform(0, MAX_DELAY))
            text = DONE if tail is not None and DONE in tail["source_text"] else ANSWER
            try:
                conn.sendall(turn_answer(text))
            except OSError:
                harness.count("gateway_cut")


class Channel:
    """The fake channel: holds ``inbox_turns`` for the owner session, reads each handed-over turn
    to its ``end``, reports its delivery and holds again. Its send always succeeds and it never
    reports ``silent``."""

    def __init__(self, harness: Harness) -> None:
        self.harness = harness
        threading.Thread(target=self.run, daemon=True).start()

    def run(self) -> None:
        harness = self.harness
        while not harness.stopping.is_set():
            harness.engine_up.wait()
            try:
                held, _ = call(harness.engine_sock, "inbox_turns", {"session": OWNER}, None)
                turn = held.get("turn")
                if turn is not None:
                    self.take(turn)
            except (Failure, OSError):
                harness.count("channel_cut")
                time.sleep(RETRY_SECONDS)

    def take(self, turn: str) -> None:
        harness = self.harness
        after, final, outcome = 0, None, None
        while outcome is None:
            page, _ = call(harness.engine_sock, "turn_events",
                           {"session": OWNER, "turn": turn, "after": after}, None)
            if not page["events"] and page["state"] not in ("queued", "running"):
                return
            for event in page["events"]:
                after += 1
                if event["type"] == "final":
                    final = event["text"]
                elif event["type"] == "end":
                    outcome = event["outcome"]
                    break
        harness.seen("turn ended")
        if harness.fire("K4") or harness.fire("K8"):
            return
        report = {"session": OWNER, "turn": turn, "delivery": delivery_of(outcome, final)}
        ack, _ = call(harness.engine_sock, "report_delivery", report)
        if ack["ok"]:
            harness.fire("K9")


class Harness:
    """The processes of one test root, the kill points and the ledger of the events."""

    def __init__(self, inbox: str, engine: str, root: Path, seed: int) -> None:
        self.inbox_bin, self.engine_bin, self.root = inbox, engine, root
        self.store = root / "inbox"
        self.inbox_sock = str(root / "inbox.sock")
        self.proxy_sock = str(root / "proxy.sock")
        self.engine_sock = str(root / "c6" / "engine.sock")
        self.journal = root / "state" / "journal.sqlite3"
        self.seed = seed
        self.env = {k: v for k, v in os.environ.items() if k not in SETTING_ENVS}
        self.ledger = Ledger()
        self.proxy = Proxy(self)
        self.engine_up = threading.Event()
        self.stopping = threading.Event()
        self.gateway_held = threading.Event()
        self.arm_lock = threading.Lock()
        self.point: str | None = None
        self.fired = threading.Event()
        self.phase = "none"
        self.counts: Counter[str] = Counter()
        self.forced_kills: dict[str, str] = {}
        self.random_kills: Counter[str] = Counter()
        self.random_phases: Counter[str] = Counter()
        self.expected: dict[str, str] = {}
        self.failures: list[str] = []
        self.consumer: subprocess.Popen | None = None
        self.engine: subprocess.Popen | None = None
        self.key = b""

    def seen(self, phase: str) -> None:
        self.phase = phase

    def count(self, name: str) -> None:
        with self.arm_lock:
            self.counts[name] += 1

    def arm(self, point: str) -> None:
        with self.arm_lock:
            self.point = point
            self.fired = threading.Event()
            self.gateway_held = threading.Event()

    def disarm(self) -> None:
        with self.arm_lock:
            self.point = None

    def armed(self, point: str) -> bool:
        return self.point == point and not self.fired.is_set()

    def fire(self, point: str) -> bool:
        """Kills the process of ``point`` when it is armed and has not fired; whether it did."""
        with self.arm_lock:
            if self.point != point or self.fired.is_set():
                return False
            self.kill((POINTS[point][0],))
            self.forced_kills[point] = self.phase
            self.fired.set()
            return True

    def wait_fired(self) -> bool:
        return self.fired.wait(FIRE_SECONDS)

    def kill(self, targets: tuple[str, ...]) -> None:
        if "consumer" in targets:
            self.stop_consumer()
        if "engine" in targets:
            self.stop_engine()

    def alive(self, daemon: subprocess.Popen, name: str) -> None:
        if daemon.poll() is not None:
            self.failures.append(f"{name} exited with status {daemon.returncode} on its own")

    def stop_consumer(self) -> None:
        if self.consumer is not None:
            self.alive(self.consumer, "consumer")
            stop(self.consumer)
            self.consumer = None

    def stop_engine(self) -> None:
        self.engine_up.clear()
        if self.engine is not None:
            self.alive(self.engine, "engine")
            stop(self.engine)
            self.engine.stdout.close()
            self.engine.stderr.close()
            self.engine = None
        self.proxy.unbind()

    def start(self, targets: tuple[str, ...], rng: random.Random) -> None:
        """Starts each of ``targets`` again after a uniform 0 to 500 ms pause."""
        for target in targets:
            time.sleep(rng.uniform(0, MAX_PAUSE))
            if target == "consumer":
                self.start_consumer()
            else:
                self.start_engine()

    def start_consumer(self) -> None:
        self.consumer, _ = spawn(self.inbox_bin, self.store, self.inbox_sock,
                                 ("--deliver", self.proxy_sock, "--deliver-tick-ms", TICK_MS))

    def start_engine(self) -> None:
        engine = subprocess.Popen(
            [self.engine_bin, "serve", "--config", str(self.root / "ulei.toml")],
            cwd=self.root, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
        try:
            ready(engine)
        except BaseException:
            if engine.returncode is None:
                engine.kill()
                engine.communicate()
            raise
        self.engine = engine
        self.proxy.bind()
        self.engine_up.set()

    def publish(self, n: int) -> str:
        """Publishes event ``n``, repeating the same packet after a failed call."""
        eid = event_id(n)
        payload = f"Kill test event {n}." + (f" Reply {DONE}." if n % 10 == 0 else "")
        packet = sign({"version": 1, "kind": "task_result", "id": eid, "payload": payload,
                       "source_ts": int(time.time()), "trust": "untrusted-source-data"}, self.key)
        deadline = time.monotonic() + PUBLISH_SECONDS
        while True:
            try:
                answer, _ = call(self.inbox_sock, "publish", {"packet": packet})
                break
            except (Failure, OSError) as e:
                if time.monotonic() > deadline:
                    raise Failure(f"publish of {eid}: {e}") from None
                time.sleep(RETRY_SECONDS)
        if answer != {"id": eid}:
            raise Failure(f"publish of {eid} answered {answer}")
        self.ledger.publish(eid, time.monotonic())
        return eid

    def refresh(self) -> bool:
        """Starts a new session for the owner key; false when the engine does not take it."""
        try:
            answer, _ = call(self.engine_sock, "submit_turn",
                             {"session": OWNER, "new": True, "prompt": REFRESH})
        except (Failure, OSError):
            return False
        return answer["accepted"]

    def quiesce(self) -> None:
        """Waits until the owner session runs and queues no turn, so the final kill cuts no
        journal write."""
        deadline = time.monotonic() + FIRE_SECONDS
        while True:
            status, _ = call(self.engine_sock, "status", {"session": OWNER})
            if not status["running"] and status["queue_depth"] == 0:
                return
            if time.monotonic() > deadline:
                raise Failure(f"the owner session still runs a turn after {FIRE_SECONDS} s")
            time.sleep(RETRY_SECONDS)

    def row(self, eid: str) -> dict:
        """The C1 row of ``eid`` by `get`; empty when the store has none."""
        answer, _ = call(self.inbox_sock, "get", {"id": eid})
        return answer.get("row", {})

    def poll(self) -> None:
        """Settles the published events from C1 `get` and refreshes the owner session after
        every 50 settled events."""
        refreshed = 0
        while not self.stopping.is_set():
            time.sleep(POLL_SECONDS)
            for eid in self.ledger.unsettled():
                try:
                    row = self.row(eid)
                except (Failure, OSError):
                    break
                if row.get("status") in SETTLED:
                    self.ledger.settle(eid, row, time.monotonic())
            due = len(self.ledger.settled) // REFRESH_EVERY
            if due > refreshed and self.refresh():
                refreshed = due

    def forced(self) -> None:
        """Each point on one event published alone: armed, killed, restarted, settled."""
        rng = random.Random(f"{self.seed}:forced")
        for n, (point, (target, status, _)) in enumerate(POINTS.items(), start=1):
            began = time.monotonic()
            self.arm(point)
            eid = self.publish(n)
            self.expected[eid] = point
            hit = self.wait_fired()
            self.disarm()
            if hit:
                self.start((target,), rng)
            else:
                self.failures.append(f"{point} was not hit within {FIRE_SECONDS} s")
            if not self.ledger.wait(lambda eid=eid: eid in self.ledger.settled, DRAIN_SECONDS):
                self.failures.append(f"{point}: {eid} unsettled after {DRAIN_SECONDS} s")
                continue
            row = self.ledger.settled[eid][0]
            if row["status"] != status:
                self.failures.append(f"{point}: {eid} ended {row['status']}, expected {status}")
            progress(f"{point} {eid} ended {row['status']}/{row.get('outcome')} after "
                     f"{time.monotonic() - began:.0f} s")

    def scheduled(self, start: float, duration: float, kills: int) -> None:
        """Kills at seeded exponential gaps spread over ``duration`` from ``start``: the consumer,
        the engine or both, each started again."""
        rng = random.Random(f"{self.seed}:kills")
        gaps = [rng.expovariate(1.0) for _ in range(kills + 1)]
        total = sum(gaps)
        at = 0.0
        for gap in gaps[:-1]:
            at += gap
            if self.stopping.wait(max(0.0, start + at / total * duration - time.monotonic())):
                return
            draw = rng.random()
            if draw < CONSUMER_SHARE:
                target, targets = "consumer", ("consumer",)
            elif draw < CONSUMER_SHARE + ENGINE_SHARE:
                target, targets = "engine", ("engine",)
            else:
                target, targets = "both", ("consumer", "engine")
            with self.arm_lock:
                self.random_phases[self.phase] += 1
                self.random_kills[target] += 1
                self.kill(targets)
            self.start(targets, rng)

    def random_phase(self, first: int, events: int, kills: int) -> None:
        """Publishes events ``first`` to ``events`` at ``RATE`` per second while the scheduler
        kills ``kills`` times."""
        duration = (events - first + 1) / RATE
        start = time.monotonic()
        errors: list[BaseException] = []

        def schedule() -> None:
            try:
                self.scheduled(start, duration, kills)
            except BaseException as e:
                errors.append(e)

        scheduler = threading.Thread(target=schedule, daemon=True)
        scheduler.start()
        try:
            due = start
            for n in range(first, events + 1):
                due = max(due + 1 / RATE, time.monotonic())
                time.sleep(max(0.0, due - time.monotonic()))
                if not self.ledger.wait(
                    lambda: len(self.ledger.unsettled()) < MAX_UNSETTLED, DRAIN_SECONDS
                ):
                    raise Failure(f"{MAX_UNSETTLED} events unsettled for {DRAIN_SECONDS} s")
                self.publish(n)
                if errors:
                    raise errors[0]
        except BaseException:
            self.stopping.set()
            raise
        finally:
            scheduler.join()
        if errors:
            raise errors[0]

    def turns(self) -> dict[str, set[tuple[int, int]]]:
        """The distinct (session, turn) of each event id in the owner key's history, from the
        journal opened read-only; a compacted message fails the run."""
        with closing(sqlite3.connect(f"file:{self.journal}?mode=ro", uri=True)) as db:
            compacted = db.execute("SELECT count(*) FROM messages WHERE compacted != 0").fetchone()
            if compacted[0]:
                self.failures.append(f"the journal holds {compacted[0]} compacted messages")
            rows = db.execute(
                "SELECT m.session, m.turn, m.message FROM messages m "
                "JOIN sessions s ON s.id = m.session WHERE s.key = ?", (OWNER,)
            ).fetchall()
        found: dict[str, set[tuple[int, int]]] = {}
        for session, turn, text in rows:
            message = json.loads(text)
            if message["role"] != "user":
                continue
            tail = tail_of(text_of(message))
            if tail is not None:
                found.setdefault(tail["event_id"], set()).add((session, turn))
        return found

    def run(self, events: int, kills: int) -> dict[str, int | float | str]:
        started = time.monotonic()
        self.store.mkdir(mode=0o700)
        (self.store / "ENABLED").touch(mode=0o600)
        config = write_config(self.root, self.root / "qb.socket", None)
        serve_config(config, Path(self.engine_sock), OWNER)
        Gateway(self, self.root / "qb.socket", random.Random(f"{self.seed}:gateway"))
        self.start_engine()
        Channel(self)
        self.start_consumer()
        self.key = (self.store / "producer.key").read_bytes()
        poller = threading.Thread(target=self.poll, daemon=True)
        poller.start()
        deadline = time.monotonic() + PUBLISH_SECONDS
        while not self.refresh():
            if time.monotonic() > deadline:
                raise Failure("the engine took no first owner turn")
            time.sleep(RETRY_SECONDS)
        self.forced()
        progress(f"forced phase done after {time.monotonic() - started:.0f} s")
        self.random_phase(len(POINTS) + 1, events, kills)
        progress(f"random phase done after {time.monotonic() - started:.0f} s with "
                 f"{len(self.ledger.unsettled())} events unsettled")
        if not self.ledger.wait(lambda: not self.ledger.unsettled(), DRAIN_SECONDS):
            self.failures.append(
                f"{len(self.ledger.unsettled())} events unsettled after the drain")
        wall = time.monotonic() - started
        rows = {eid: self.row(eid) for eid in self.ledger.published}
        self.stopping.set()
        poller.join()
        self.quiesce()
        consumer_hwm = proc_field(self.consumer.pid, "status", "VmHWM")
        engine_hwm = proc_field(self.engine.pid, "status", "VmHWM")
        self.stop_consumer()
        self.stop_engine()
        return self.reconcile(rows, self.turns(), wall, (consumer_hwm, engine_hwm))

    def reconcile(
        self, rows: dict[str, dict], turns: dict[str, set], wall: float, hwm: tuple[int, int]
    ) -> dict[str, int | float | str]:
        """The figures of the run; a broken pass rule or forced end state is a failure."""
        lost = duplicated = 0
        statuses: Counter[str] = Counter()
        uncertain: Counter[str] = Counter()
        for eid, row in rows.items():
            status, count = row.get("status"), len(turns.get(eid, ()))
            statuses[f"{status}_{row.get('outcome')}"] += 1
            if status not in SETTLED or (status == "done" and count == 0):
                lost += 1
                self.failures.append(f"lost {eid}: {status}/{row.get('outcome')}, {count} turns")
            elif status == "done" and count > 1:
                duplicated += 1
                self.failures.append(f"duplicated {eid}: done with {count} turns")
            if status == "uncertain":
                uncertain[str(count) if count < 3 else "3plus"] += 1
            point = self.expected.get(eid)
            if point is not None and count != POINTS[point][2]:
                self.failures.append(
                    f"{point}: {eid} has {count} turns, expected {POINTS[point][2]}")
        phantom = sorted(set(turns) - set(rows))
        if phantom:
            self.failures.append(f"turns of events never published: {', '.join(phantom)}")
        settle = sorted(at - self.ledger.published[eid]
                        for eid, (_, at) in self.ledger.settled.items())
        figures: dict[str, int | float | str] = {
            "seed": self.seed,
            "events": len(rows),
            "forced_kills": len(self.forced_kills),
            "random_kills": sum(self.random_kills.values()),
        }
        for point in POINTS:
            figures[f"kills_{point}"] = int(point in self.forced_kills)
        for point, phase in self.forced_kills.items():
            figures[f"phase_{point}"] = slug(phase)
        for target in ("consumer", "engine", "both"):
            figures[f"kills_{target}"] = self.random_kills[target]
        for phase in ("none", *PHASES):
            figures[f"kills_{slug(phase)}"] = self.random_phases[phase]
        for key in sorted(statuses):
            figures[f"rows_{key}"] = statuses[key]
        figures["lost"] = lost
        figures["duplicated"] = duplicated
        figures["phantom"] = len(phantom)
        for count in ("0", "1", "2", "3plus"):
            figures[f"uncertain_turns_{count}"] = uncertain[count]
        figures["wall_s"] = round(wall, 1)
        figures["events_per_s"] = round(len(rows) / wall, 2)
        figures["settle_p50_ms"] = int(settle[len(settle) // 2] * 1000) if settle else -1
        figures["settle_p99_ms"] = (
            int(settle[max(0, len(settle) * 99 // 100 - 1)] * 1000) if settle else -1)
        figures["consumer_vmhwm_kb"], figures["engine_vmhwm_kb"] = hwm
        figures["journal_bytes"] = self.journal.stat().st_size
        for name in ("gateway_cut", "channel_cut"):
            figures[name] = self.counts[name]
        return figures


def annotate(level: str, title: str, text: str) -> None:
    """One GitHub annotation; GitHub keeps it even when the step later times out."""
    escaped = text.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::{level} title={title}::{escaped}", flush=True)


def progress(text: str) -> None:
    annotate("notice", "deliver-kill progress", text)


def slug(phase: str) -> str:
    return phase.replace(" ", "_")


def summary(figures: dict[str, int | float | str]) -> str:
    """The figures as one line of text."""
    forced = ", ".join(f"{p} {figures['phase_' + p]}" for p in POINTS if f"phase_{p}" in figures)
    targets = ", ".join(f"{t} {figures['kills_' + t]}" for t in ("consumer", "engine", "both"))
    phases = ", ".join(f"{p} {figures['kills_' + slug(p)]}" for p in ("none", *PHASES))
    rows = ", ".join(f"{k.removeprefix('rows_')} {v}" for k, v in figures.items()
                     if k.startswith("rows_"))
    uncertain = ", ".join(f"{c} turns {figures['uncertain_turns_' + c]}"
                          for c in ("0", "1", "2", "3plus"))
    return (
        f"seed {figures['seed']}; events {figures['events']}; "
        f"forced kills {figures['forced_kills']} by point and phase ({forced}); "
        f"random kills {figures['random_kills']} ({targets}) by phase ({phases}); "
        f"rows by status_outcome ({rows}); lost {figures['lost']}; "
        f"duplicated {figures['duplicated']}; turns of unpublished ids {figures['phantom']}; "
        f"uncertain rows ({uncertain}); wall {figures['wall_s']} s; "
        f"{figures['events_per_s']} events/s; publish to settle p50 {figures['settle_p50_ms']} ms "
        f"p99 {figures['settle_p99_ms']} ms; VmHWM consumer {figures['consumer_vmhwm_kb']} kB "
        f"engine {figures['engine_vmhwm_kb']} kB; journal {figures['journal_bytes']} B"
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(prog="deliver_kill.py")
    parser.add_argument("inbox")
    parser.add_argument("engine")
    parser.add_argument("workdir")
    parser.add_argument("--events", type=int, required=True)
    parser.add_argument("--kills", type=int, required=True)
    parser.add_argument("--seed", type=int, required=True)
    args = parser.parse_args(argv)
    if args.events <= len(POINTS) or args.kills < 0:
        parser.error(f"--events must exceed {len(POINTS)} and --kills must not be negative")
    root = Path(tempfile.mkdtemp(prefix="kill-", dir=args.workdir))
    harness = Harness(os.path.abspath(args.inbox), os.path.abspath(args.engine), root, args.seed)
    try:
        figures = harness.run(args.events, args.kills)
    except (Failure, OSError, ValueError, sqlite3.Error) as e:
        for failure in (str(e), *harness.failures):
            annotate("error", "deliver-kill", failure)
        return 1
    finally:
        harness.stopping.set()
        if harness.consumer is not None:
            stop(harness.consumer)
        if harness.engine is not None:
            stop(harness.engine)
    for name, value in figures.items():
        print(name, value)
    text = summary(figures)
    annotate("notice", "deliver-kill", text)
    step_summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if step_summary:
        with open(step_summary, "a", encoding="utf-8") as f:
            f.write(f"### Delivery kill test\n\n{text}\n")
    for failure in harness.failures:
        annotate("error", "deliver-kill", failure)
    return 1 if harness.failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
