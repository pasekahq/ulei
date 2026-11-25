"""Driver of the `ulei-priv` budgets (ADR-0073) on the static musl release binary.

``python3 ci/priv_run.py ULEI_PRIV WORKDIR`` is the `ulei-priv` section of ``ci/budgets.sh``: it
starts ``ulei-priv serve`` as the runner's user on a policy of WORKDIR that allows its uid
``/bin/echo``, sends ``CALLS`` requests alternating ``run`` and ``list`` over the socket, checks
one audit line per call, then reads the voluntary context switches of all threads of ``serve``
over ``IDLE_SECONDS`` and its VmHWM. It prints one ``<name> <value>`` line per measurement.
"""

from __future__ import annotations

import json
import os
import select
import socket
import subprocess
import sys
import time
from pathlib import Path

from inbox_load import Failure, proc_field, stop, thread_switches

CALLS = 200
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY = b"ulei-priv: ready: "
READY_TIMEOUT = 10.0


def request(path: str, body: dict) -> bytes:
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(10)
        conn.connect(path)
        conn.sendall(json.dumps(body).encode() + b"\n")
        reply = b""
        while chunk := conn.recv(65536):
            reply += chunk
    return reply


def measure(priv: str, work: Path) -> dict[str, int]:
    root = work / "priv"
    root.mkdir(mode=0o700)
    policy = root / "priv.toml"
    policy.write_text(
        f'allowed_uids = [{os.getuid()}]\n\n[[command]]\nid = "echo"\n'
        'argv = ["/bin/echo", "hello"]\n',
        encoding="ascii",
    )
    policy.chmod(0o644)
    sock, audit = root / "priv.sock", root / "audit.jsonl"
    daemon = subprocess.Popen(
        [priv, "serve", "--policy", str(policy), "--socket", str(sock), "--audit", str(audit)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        said = b""
        deadline = time.monotonic() + READY_TIMEOUT
        while READY not in said:
            left = deadline - time.monotonic()
            if left <= 0 or not select.select([daemon.stderr], [], [], left)[0]:
                raise Failure(f"no ready line within {READY_TIMEOUT} s: {said!r}")
            chunk = os.read(daemon.stderr.fileno(), 65536)
            if not chunk:
                raise Failure(f"serve exited with status {daemon.wait()}: {said!r}")
            said += chunk
        for n in range(CALLS):
            body = {"op": "run", "id": "echo", "args": []} if n % 2 == 0 else {"op": "list"}
            reply = request(str(sock), body)
            if json.loads(reply.split(b"\n", 1)[0]).get("decision") != "ok":
                raise Failure(f"call {n}: {reply[:200]!r}")
            # Keep the audit lines on stderr drained.
            while select.select([daemon.stderr], [], [], 0)[0]:
                if not os.read(daemon.stderr.fileno(), 65536):
                    break
        lines = audit.read_text(encoding="utf-8").splitlines()
        if len(lines) != CALLS:
            raise Failure(f"{len(lines)} audit lines for {CALLS} calls")
        time.sleep(SETTLE_SECONDS)
        before = thread_switches(daemon.pid)
        time.sleep(IDLE_SECONDS)
        after = thread_switches(daemon.pid)
        if daemon.poll() is not None:
            raise Failure(f"serve exited with status {daemon.returncode} while idle")
        hwm = proc_field(daemon.pid, "status", "VmHWM")
    finally:
        stop(daemon)
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    return {
        "calls": CALLS,
        "vmhwm_kb": hwm,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print("usage: ci/priv_run.py ULEI_PRIV WORKDIR", file=sys.stderr)
        return 2
    try:
        for name, value in measure(argv[1], Path(argv[2])).items():
            print(name, value)
    except Failure as e:
        print(f"ulei-priv run: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
