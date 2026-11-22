"""Driver of the `session_search` line of the `ulei-engine` budgets in ``ci/budgets.sh``
(ADR-0051 section 7).

``python3 ci/session_search_run.py ULEI_ENGINE WORKDIR`` runs ``ULEI_ENGINE serve`` against a
fake gateway, fills a fresh journal with ``FILL_SESSIONS`` sessions of ``FILL_MESSAGES``
messages through C6 `submit_turn`, then runs search turns in which the gateway asks for
``CALLS`` `session_search` calls in turn discovery, scroll and read. A call's latency is the
time from the gateway's answer carrying the call to the engine's connect for its next request,
whose body the engine serializes before connecting. It prints one ``<name> <value>`` line per
measurement. A failed call, a wrong fill or a turn that does not end is a failure.
"""

from __future__ import annotations

import json
import os
import socket
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
from contextlib import closing
from pathlib import Path

from engine_idle import (
    POLL_SECONDS,
    SETTING_ENVS,
    exited,
    quiesce,
    read_turn,
    ready,
    serve_config,
    skills,
    text_of,
    turn_stream,
    write_config,
)
from garden_run import ACTIVITIES, PLACES, fact, p50, query
from inbox_load import Failure, call

FILL_SESSIONS = 100
FILL_MESSAGES = 100
# A turn journals its prompt and the assistant's answer.
TURNS_PER_SESSION = FILL_MESSAGES // 2
FILL_TURNS = FILL_SESSIONS * TURNS_PER_SESSION
CALLS = 200
SHAPES = ("discovery", "scroll", "read")
FILL_KEY = "budget"
SEARCH_KEY = "budget-search"
SEARCH_PROMPT = "Budget search turn"
SEARCH_ANSWER = "Search turn done."
PROMPT_TOKENS = 1000
SUBMIT_SECONDS = 30.0
TURN_SECONDS = 30.0
# The words of fill and search prompts besides the facts, none of them a query word.
USER_FILLER = (
    "Keep this synthetic owner note in the session history so the budget run has messages "
    "of a realistic length."
)
ASSISTANT_FILLER = (
    "I have filed this synthetic note with the others of this session and will bring it back "
    "when the owner asks about it again. "
) * 4
# Single words first, each once, then activity and place pairs, each once: no query matches
# the tool call row an earlier search turn journaled, so every hit is a filled session.
SINGLE_WORDS = [word for _, word in ACTIVITIES] + [word for _, word in PLACES]


def fill_prompt(turn: int) -> str:
    return f"Note {turn}: someone {fact(turn)}. {USER_FILLER}"


def discovery_query(n: int) -> str:
    return SINGLE_WORDS[n] if n < len(SINGLE_WORDS) else query(n)


class Gateway:
    """The fake C4G gateway on ``qb.socket``. It answers a fill prompt with one assistant text,
    and a search prompt with a discovery call, a discovery result with a scroll of its first
    session, a scroll result with a read of its last session and a read result with the end of
    the turn, until ``CALLS`` calls are timed."""

    def __init__(self, path: Path) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(8)
        self.filled = 0
        self.discoveries = 0
        self.gaps: dict[str, list[float]] = {shape: [] for shape in SHAPES}
        self.pending: tuple[str, str, float] | None = None
        self.targets: list[dict] = []
        self.errors: list[str] = []
        self.turn_done = threading.Event()
        threading.Thread(target=self.serve, daemon=True).start()

    def calls(self) -> int:
        return sum(len(gaps) for gaps in self.gaps.values())

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            arrived = time.perf_counter()
            with conn:
                try:
                    self.answer(conn, arrived)
                except (Failure, OSError, ValueError, KeyError, IndexError, TypeError) as e:
                    self.errors.append(f"fake gateway: {e!r}")
                    self.turn_done.set()

    def answer(self, conn: socket.socket, arrived: float) -> None:
        last = read_turn(conn)["messages"][-1]
        results = [block for block in last["content"] if block["type"] == "tool_result"]
        if results:
            self.after_call(conn, arrived, results)
            return
        text = text_of(last)
        if SEARCH_PROMPT in text:
            self.discoveries += 1
            self.send_call(conn, "discovery", {"query": discovery_query(self.discoveries - 1)})
            return
        noted = text.split(". ", 1)[0].removeprefix("Note ")
        self.end_turn(conn, f"Noted {noted}. {ASSISTANT_FILLER}")
        self.filled += 1

    def send_call(self, conn: socket.socket, shape: str, args: dict) -> None:
        call_id = f"call-{self.calls()}"
        conn.sendall(turn_stream(
            {"type": "tool_call",
             "tool_call": {"id": call_id, "name": "session_search", "input": args}},
            "tool_use", PROMPT_TOKENS,
        ))
        self.pending = (shape, call_id, time.perf_counter())

    def end_turn(self, conn: socket.socket, text: str) -> None:
        conn.sendall(turn_stream({"type": "text_delta", "text": text}, "end", PROMPT_TOKENS))

    def after_call(self, conn: socket.socket, arrived: float, results: list[dict]) -> None:
        if self.pending is None:
            raise Failure("a tool result without a pending call")
        shape, call_id, sent = self.pending
        self.pending = None
        if [block["id"] for block in results] != [call_id]:
            raise Failure(f"the {shape} call {call_id} answered by {[b['id'] for b in results]}")
        content = results[0]["content"]
        output = json.loads(content)
        if results[0].get("is_error") or output["success"] is not True:
            raise Failure(f"the {shape} call {call_id} failed: {content[:300]}")
        self.gaps[shape].append(arrived - sent)
        follow = None if self.calls() >= CALLS else self.follow(shape, output)
        if follow is None:
            self.end_turn(conn, SEARCH_ANSWER)
            self.turn_done.set()
        else:
            self.send_call(conn, *follow)

    def follow(self, shape: str, output: dict) -> tuple[str, dict] | None:
        """The next call of the search turn after the ``shape`` call answered ``output``."""
        if shape == "discovery":
            if not output["results"]:
                raise Failure(f"discovery {output['query']!r} found no session")
            self.targets = output["results"]
            first = self.targets[0]
            return "scroll", {"session_id": first["session_id"],
                              "around_message_id": first["match_message_id"]}
        if shape == "scroll":
            if not output["messages"]:
                raise Failure(f"scroll of session {output['session_id']} returned no messages")
            return "read", {"session_id": self.targets[-1]["session_id"]}
        if output["message_count"] != FILL_MESSAGES:
            raise Failure(f"read of session {output['session_id']} counted "
                          f"{output['message_count']} messages, expected {FILL_MESSAGES}")
        return None

    def check(self) -> None:
        if self.errors:
            raise Failure("; ".join(self.errors))

    def close(self) -> None:
        self.listener.close()


def submit(path: str, request: dict) -> None:
    """Submits ``request`` as `submit_turn`, again while the session's queue is full."""
    deadline = time.monotonic() + SUBMIT_SECONDS
    while True:
        answer, _ = call(path, "submit_turn", request)
        if answer["accepted"]:
            return
        if time.monotonic() > deadline:
            raise Failure(f"submit_turn refused for {SUBMIT_SECONDS} s: {answer}")
        time.sleep(POLL_SECONDS)


def fill(path: str, gateway: Gateway) -> float:
    """Fills the journal through `submit_turn`; returns the seconds until the last turn ended."""
    started = time.monotonic()
    for turn in range(FILL_TURNS):
        submit(path, {"session": FILL_KEY, "prompt": fill_prompt(turn),
                      "new": turn % TURNS_PER_SESSION == 0})
        gateway.check()
    quiesce(path, FILL_KEY, TURN_SECONDS)
    seconds = time.monotonic() - started
    gateway.check()
    if gateway.filled != FILL_TURNS:
        raise Failure(f"the gateway answered {gateway.filled} fill turns, expected {FILL_TURNS}")
    return seconds


def filled_sessions(journal: Path) -> list[int]:
    """The message count of each session of the fill key, from the journal opened read-only."""
    with closing(sqlite3.connect(f"file:{journal}?mode=ro", uri=True)) as db:
        rows = db.execute(
            "SELECT count(*) FROM messages m JOIN sessions s ON s.id = m.session "
            "WHERE s.key = ? GROUP BY m.session", (FILL_KEY,)
        ).fetchall()
    return [count for (count,) in rows]


def search(path: str, gateway: Gateway, engine: subprocess.Popen) -> None:
    """Runs search turns, each in a new session, until the gateway timed ``CALLS`` calls."""
    turn = 0
    while gateway.calls() < CALLS:
        gateway.turn_done.clear()
        submit(path, {"session": SEARCH_KEY, "prompt": f"{SEARCH_PROMPT} {turn}.", "new": True})
        if not gateway.turn_done.wait(TURN_SECONDS):
            if engine.poll() is not None:
                raise exited(engine, f"during search turn {turn}")
            raise Failure(f"search turn {turn} did not end within {TURN_SECONDS} s")
        gateway.check()
        quiesce(path, SEARCH_KEY, TURN_SECONDS)
        turn += 1


def measure(engine_bin: str, work: Path) -> dict[str, int | str]:
    gateway_path = work / "gateway.sock"
    config = write_config(work, gateway_path, skills(work))
    c6 = work / "c6"
    c6.mkdir(mode=0o700)
    path = str(c6 / "engine.sock")
    serve_config(config, Path(path), FILL_KEY)
    env = {k: v for k, v in os.environ.items() if k not in SETTING_ENVS}
    gateway = Gateway(gateway_path)
    engine = subprocess.Popen(
        [engine_bin, "serve", "--config", str(config)],
        cwd=work, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        ready(engine)
        fill_seconds = fill(path, gateway)
        counts = filled_sessions(work / "state/journal.sqlite3")
        if len(counts) != FILL_SESSIONS or set(counts) != {FILL_MESSAGES}:
            raise Failure(f"the fill journaled sessions of {sorted(set(counts))} messages, "
                          f"{len(counts)} sessions, expected {FILL_SESSIONS} of {FILL_MESSAGES}")
        search(path, gateway, engine)
        if engine.poll() is not None:
            raise exited(engine, "after the search turns")
    finally:
        if engine.returncode is None:
            engine.kill()
            engine.communicate()
        gateway.close()
    gaps = gateway.gaps
    return {
        "fill_sessions": len(counts),
        "fill_messages": sum(counts),
        "fill_turns": FILL_TURNS,
        "fill_seconds": f"{fill_seconds:.1f}",
        "search_calls": gateway.calls(),
        "search_p50_us": p50([gap for shape in SHAPES for gap in gaps[shape]]),
        **{f"{shape}_calls": len(gaps[shape]) for shape in SHAPES},
        **{f"{shape}_p50_us": p50(gaps[shape]) for shape in SHAPES},
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: session_search_run.py ULEI_ENGINE WORKDIR", file=sys.stderr)
        return 2
    try:
        with tempfile.TemporaryDirectory(prefix="session-search-", dir=argv[1]) as work:
            results = measure(os.path.abspath(argv[0]), Path(work))
    except (Failure, OSError, ValueError, sqlite3.Error) as e:
        print(f"session_search_run: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
