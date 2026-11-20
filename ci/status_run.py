"""Driver of the `ulei status` line of the `ulei-inbox` budgets in ``ci/budgets.sh`` (ADR-0040
section 6).

``python3 ci/status_run.py ULEI WORKDIR`` serves fake C1, C6 and C4G daemons on Unix sockets
under WORKDIR, all up, writes a synthetic ``ulei.toml`` naming them and runs ``ULEI status``
once under ``strace -f -c`` and once under ``/usr/bin/time``. It prints one ``<name> <value>``
line per measurement. A status that does not exit 0 is a failure.
"""

from __future__ import annotations

import json
import socket
import subprocess
import sys
import threading
from pathlib import Path


class Failure(Exception):
    """A run that cannot yield valid budget numbers."""


ANSWERS = {
    "inbox": {
        "POST /v1/health": {"ok": True, "version": "ulei-inbox budget"},
        "POST /v1/counts": {"pending": 0, "leased": 0, "processing": 0, "deferred": 0,
                            "uncertain": 0},
    },
    "engine": {
        "POST /v1/is_idle": {"idle": True, "since": 1.0, "channel": True},
        "POST /v1/status": {"session": "budget-session", "running": False, "since": 1.0,
                            "queue_depth": 0, "route": "main"},
    },
    "qb": {
        "GET /v1/health": {"status": "ok", "api_version": "v1", "version": "budget",
                           "uptime_s": 1},
        "GET /v1/model/routes": {"routes": [{"alias": "main", "state": "ready",
                                             "vendor": "vendor-a", "model": "model-a"}]},
    },
}


def serve(listener: socket.socket, answers: dict[str, dict]) -> None:
    """Answers each request on ``listener`` from ``answers`` by its request line, until closed."""
    while True:
        try:
            conn, _ = listener.accept()
        except OSError:
            return
        with conn:
            target = read_target(conn)
            body = json.dumps(answers[target]).encode()
            conn.sendall(
                f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body
            )


def read_target(conn: socket.socket) -> str:
    """``METHOD /path`` of one request on ``conn``, its head and its Content-Length body read."""
    received = b""
    while b"\r\n\r\n" not in received:
        chunk = conn.recv(4096)
        if not chunk:
            raise Failure("request cut before its head ended")
        received += chunk
    head, body = received.split(b"\r\n\r\n", 1)
    lines = head.decode().split("\r\n")
    length = sum(int(line.split(":", 1)[1]) for line in lines[1:]
                 if line.lower().startswith("content-length:"))
    while len(body) < length:
        chunk = conn.recv(4096)
        if not chunk:
            raise Failure("request cut before its body ended")
        body += chunk
    return lines[0].rsplit(" ", 1)[0]


def strace_counts(trace: Path) -> tuple[int, int]:
    """All syscalls and the open calls of an ``strace -c`` summary."""
    calls = None
    opens = 0
    for line in trace.read_text(encoding="ascii").splitlines():
        fields = line.split()
        if len(fields) < 5 or not fields[3].isdigit():
            continue
        if fields[-1] == "total":
            calls = int(fields[3])
        elif fields[-1].startswith("open"):
            opens += int(fields[3])
    if calls is None:
        raise Failure(f"{trace} has no total line")
    return calls, opens


def run(command: list[str]) -> subprocess.CompletedProcess:
    done = subprocess.run(command, capture_output=True, text=True, check=False, timeout=30)
    if done.returncode != 0:
        raise Failure(f"{' '.join(command)} exited {done.returncode}: {done.stdout}{done.stderr}")
    return done


def measure(ulei: str, work: Path) -> dict[str, int]:
    paths = {name: str(work / f"status-{name}.sock") for name in ANSWERS}
    config = work / "status.toml"
    config.write_text(
        f'[inbox]\nsocket = "{paths["inbox"]}"\n\n'
        f'[engine]\nc6_socket = "{paths["engine"]}"\nowner_session = "budget-session"\n\n'
        f'[qb]\nsocket = "{paths["qb"]}"\n',
        encoding="utf-8",
    )
    listeners = []
    try:
        for name, answers in ANSWERS.items():
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listeners.append(listener)
            listener.bind(paths[name])
            listener.listen(8)
            threading.Thread(target=serve, args=(listener, answers), daemon=True).start()
        status = [ulei, "status", "--config", str(config)]
        trace = work / "status.strace"
        run(["strace", "-f", "-c", "-o", str(trace), *status])
        calls, opens = strace_counts(trace)
        timed = run(["/usr/bin/time", "-f", "%M", *status])
        rss = int(timed.stderr.strip().splitlines()[-1])
    finally:
        for listener in listeners:
            listener.close()
    return {"status_syscalls": calls, "status_opens": opens, "status_rss_kb": rss}


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: status_run.py ULEI WORKDIR", file=sys.stderr)
        return 2
    try:
        results = measure(argv[0], Path(argv[1]))
    except (Failure, OSError, subprocess.SubprocessError, ValueError) as e:
        print(f"status_run: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
