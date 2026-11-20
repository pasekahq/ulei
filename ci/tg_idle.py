"""Idle driver of the `ulei-tg` budget in ``ci/budgets.sh`` (ADR-0017 section 4).

``python3 ci/tg_idle.py ULEI_TG WORKDIR`` runs ``ULEI_TG`` on a synthetic config against a
loopback fake Bot API that holds ``getUpdates`` unanswered and a fake engine socket that holds
``inbox_turns`` unanswered (ADR-0036 section 9), measures the blocked process over all its
threads, stops it with SIGTERM and prints one ``<name> <value>`` line per measurement.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from inbox_load import Failure, proc_field, thread_switches

IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
POLL_HELD_TIMEOUT = 10.0
INBOX_HELD_TIMEOUT = 10.0
STOP_TIMEOUT = 10.0
POLL_SECONDS = 0.05
TOKEN = "123456:synthetic-budget-token"
OWNER_ID = 42
OWNER_SESSION = f"agent:main:telegram:dm:{OWNER_ID}"
BOT = {"id": 123456, "is_bot": True, "first_name": "Budget", "username": "budget_bot"}


class FakeBotApi(ThreadingHTTPServer):
    """Answers `getMe` and `deleteWebhook`; holds each `getUpdates` until ``release`` is set or
    ``poll_hold`` seconds pass, then answers no updates, or 401 once after ``revoke`` is set."""

    daemon_threads = True

    def __init__(self, poll_hold: float | None = None) -> None:
        super().__init__(("127.0.0.1", 0), BotApiHandler)
        self.poll_hold = poll_hold
        self.polls_held = threading.Event()
        self.release = threading.Event()
        self.revoke = threading.Event()
        self.errors: list[str] = []


class BotApiHandler(BaseHTTPRequestHandler):
    # Keep-alive: the client reuses one pooled connection for its calls.
    protocol_version = "HTTP/1.1"
    server: FakeBotApi

    def do_POST(self) -> None:
        self.rfile.read(int(self.headers["Content-Length"]))
        prefix = f"/bot{TOKEN}/"
        method = self.path.removeprefix(prefix) if self.path.startswith(prefix) else None
        if method == "getMe":
            self.answer(BOT)
        elif method == "deleteWebhook":
            self.answer(True)
        elif method == "getUpdates":
            self.server.polls_held.set()
            self.server.release.wait(self.server.poll_hold)
            if self.server.revoke.is_set():
                self.server.revoke.clear()
                self.reply(401, {"ok": False, "error_code": 401, "description": "Unauthorized"})
            else:
                self.answer([])
        else:
            self.server.errors.append(f"unexpected request {self.path.replace(TOKEN, '***')}")
            self.reply(404, {"ok": False, "error_code": 404, "description": "Not Found"})

    def answer(self, result: object) -> None:
        self.reply(200, {"ok": True, "result": result})

    def reply(self, status: int, envelope: dict) -> None:
        body = json.dumps(envelope).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args: object) -> None:
        pass


class FakeEngine:
    """Holds each ``inbox_turns`` for the owner session on the C6 socket ``path`` unanswered
    until ``close``; any other request is an error."""

    def __init__(self, path: Path) -> None:
        path.parent.mkdir(mode=0o700)
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen()
        self.held: list[socket.socket] = []
        self.inbox_held = threading.Event()
        self.errors: list[str] = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self.serve, args=(conn,), daemon=True).start()

    def serve(self, conn: socket.socket) -> None:
        data = b""
        while b"\r\n\r\n" not in data:
            chunk = conn.recv(4096)
            if not chunk:
                conn.close()
                return
            data += chunk
        head, _, body = data.partition(b"\r\n\r\n")
        lines = head.decode("ascii", errors="replace").split("\r\n")
        length = next(
            int(value) for name, _, value in (line.partition(":") for line in lines[1:])
            if name.lower() == "content-length"
        )
        while len(body) < length:
            chunk = conn.recv(4096)
            if not chunk:
                break
            body += chunk
        request = lines[0].split(" ")
        if request[:2] == ["POST", "/v1/inbox_turns"] and json.loads(body) == {"session": OWNER_SESSION}:
            self.held.append(conn)
            self.inbox_held.set()
            return
        self.errors.append(f"unexpected C6 request {lines[0]} {body[:200]!r}")
        conn.close()

    def close(self) -> None:
        self.listener.close()
        for conn in self.held:
            conn.close()


def config(work: Path, port: int) -> Path:
    """Writes the synthetic token file and `ulei.toml` into ``work``; returns the config."""
    token = work / "token"
    token.touch(mode=0o600)
    token.write_text(TOKEN + "\n", encoding="ascii")
    path = work / "ulei.toml"
    path.write_text(
        f'[telegram]\ntoken_file = "{token}"\nbase_url = "http://127.0.0.1:{port}/bot"\n'
        f'owner_ids = {OWNER_ID}\nattachments_dir = "{work / "attachments"}"\n\n'
        f'[engine]\nc6_socket = "{work / "c6/engine.sock"}"\n',
        encoding="utf-8",
    )
    return path


def exited(tg: subprocess.Popen, when: str) -> Failure:
    _, stderr = tg.communicate()
    line = stderr.decode(errors="replace").strip() or "no stderr"
    return Failure(f"ulei-tg exited with status {tg.returncode} {when}: {line}")


def wait_held(tg: subprocess.Popen, held: threading.Event, what: str, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while not held.wait(POLL_SECONDS):
        if tg.poll() is not None:
            raise exited(tg, f"before its first {what}")
        if time.monotonic() > deadline:
            raise Failure(f"no {what} within {timeout} s")


def switches(tg: subprocess.Popen) -> dict[int, int]:
    """Voluntary context switches of each thread of ``tg``, which must still be polling."""
    if tg.poll() is not None:
        raise exited(tg, "while idle")
    return thread_switches(tg.pid)


def stop(tg: subprocess.Popen, api: FakeBotApi) -> None:
    """SIGTERM, then the empty long-poll answer a real server gives, so the poller sees it."""
    tg.send_signal(signal.SIGTERM)
    api.release.set()
    try:
        _, stderr = tg.communicate(timeout=STOP_TIMEOUT)
    except subprocess.TimeoutExpired:
        tg.kill()
        tg.communicate()
        raise Failure(f"ulei-tg did not stop within {STOP_TIMEOUT} s of SIGTERM") from None
    if tg.returncode != 0:
        line = stderr.decode(errors="replace").strip() or "no stderr"
        raise Failure(f"ulei-tg exited with status {tg.returncode} after SIGTERM: {line}")


def measure(tg_bin: str, work: Path) -> dict[str, int]:
    api = FakeBotApi()
    threading.Thread(target=api.serve_forever, daemon=True).start()
    engine = FakeEngine(work / "c6/engine.sock")
    try:
        path = config(work, api.server_port)
        tg = subprocess.Popen(
            [tg_bin, "--config", str(path)],
            cwd=work, env={}, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        try:
            wait_held(tg, api.polls_held, "getUpdates", POLL_HELD_TIMEOUT)
            wait_held(tg, engine.inbox_held, "inbox_turns", INBOX_HELD_TIMEOUT)
            time.sleep(SETTLE_SECONDS)
            before = switches(tg)
            time.sleep(IDLE_SECONDS)
            after = switches(tg)
            hwm = proc_field(tg.pid, "status", "VmHWM")
            if before.keys() != after.keys():
                raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
            stop(tg, api)
        finally:
            api.release.set()
            if tg.returncode is None:
                tg.kill()
                tg.communicate()
    finally:
        engine.close()
        api.shutdown()
        api.server_close()
    if api.errors or engine.errors:
        raise Failure("; ".join(api.errors + engine.errors))
    if len(engine.held) != 1:
        raise Failure(f"inbox_turns was held {len(engine.held)} times, expected once")
    return {
        "vmhwm_kb": hwm,
        "idle_wakeups": sum(after[t] - before[t] for t in after),
        "idle_seconds": IDLE_SECONDS,
        "threads": len(after),
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: tg_idle.py ULEI_TG WORKDIR", file=sys.stderr)
        return 2
    try:
        with tempfile.TemporaryDirectory(prefix="tg-", dir=argv[1]) as work:
            results = measure(os.path.abspath(argv[0]), Path(work))
    except (Failure, OSError, ValueError) as e:
        print(f"tg_idle: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
