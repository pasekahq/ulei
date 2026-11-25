"""Driver of the `ulei-hive-mcp` budgets (ADR-0071 section 8) on the static musl release binary.

``python3 ci/hive_mcp_budget.py ULEI_HIVE_MCP WORKDIR`` is the `ulei-hive-mcp` section of
``ci/budgets.sh``. It writes a synthetic ``ulei.toml`` whose ``[hive_mcp]`` runs a stdlib fake of
the hive's CLI, written under WORKDIR, then measures the median from spawn to the ready line
over ``READY_RESTARTS`` starts, the voluntary context switches of all threads of ``serve`` in
``IDLE_SECONDS`` idle, and its VmHWM after ``CALLS`` sessions of the five tools (a
``hive_show`` of a card with a report of ``REPORT_BYTES``). It prints one ``<name> <value>``
line per measurement.
"""

from __future__ import annotations

import http.client
import json
import os
import select
import subprocess
import sys
import time
from pathlib import Path

from garden_run import READY_RESTARTS, alive, p50
from inbox_load import Failure, proc_field, stop, thread_switches

IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY = b"ulei-hive-mcp: ready: "
READY_TIMEOUT = 10.0
CALLS = 40
REPORT_BYTES = 256 * 1024
TOKEN = "synthetic-budget-token-0123456789abcdef"
# The fake CLI: the hive's JSON answers, a report of REPORT_BYTES on `show`.
FAKE_HIVE = """import json, sys
args = sys.argv[1:]
cmd = args[0]
if cmd == "create":
    print(json.dumps({"id": "h_0000000a", "created": True, "state": "queued"}))
elif cmd == "list":
    print(json.dumps([{"id": "h_%08x" % n, "state": "queued", "round": 0, "title": "Synthetic %d" % n}
                      for n in range(200)]))
elif cmd == "show":
    print(json.dumps({"id": args[-1], "state": "done", "round": 1, "title": "Synthetic",
                      "result": "Synthetic result.", "runs": [{"report": "# Report\\n" + "x" * REPORT}]}))
elif cmd == "steer":
    print(args[-2] + " steer 1 recorded, not delivered yet")
elif cmd == "cancel":
    print(args[-1] + " cancelled")
""".replace("REPORT", str(REPORT_BYTES))


def bed(work: Path) -> Path:
    """The config of a server on a free loopback port and the fake CLI; returns its path."""
    work.mkdir(mode=0o700, parents=True, exist_ok=True)
    fake = work / "fake_hive.py"
    fake.write_text(FAKE_HIVE, encoding="utf-8")
    token = work / "hive-mcp.token"
    token.touch(mode=0o600)
    token.write_text(TOKEN + "\n", encoding="ascii")
    config = work / "ulei.toml"
    config.write_text(
        "[hive_mcp]\n"
        'listen = "127.0.0.1:0"\n'
        f'token_file = "{token}"\n'
        f'hive_command = "{sys.executable} {fake}"\n'
        f'hive_config = "{work / "hive.toml"}"\n',
        encoding="utf-8",
    )
    return config


def spawn(server: Path, config: Path) -> tuple[subprocess.Popen, float, int]:
    """A running ``serve``, the seconds from spawn to its ready line and its port."""
    spawned = time.perf_counter()
    daemon = subprocess.Popen(
        [str(server), "serve", "--config", str(config)],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        said = b""
        while b"\n" not in said:
            left = spawned + READY_TIMEOUT - time.perf_counter()
            if left <= 0 or not select.select([daemon.stderr], [], [], left)[0]:
                raise Failure(f"no ready line within {READY_TIMEOUT} s: {said!r}")
            chunk = os.read(daemon.stderr.fileno(), 4096)
            if not chunk:
                raise Failure(f"serve exited with status {daemon.wait()} before ready: {said!r}")
            said += chunk
        ready = time.perf_counter() - spawned
        line = said.split(b"\n")[0]
        if not line.startswith(READY):
            raise Failure(f"not a ready line: {line!r}")
        return daemon, ready, int(line.rpartition(b":")[2])
    except BaseException:
        stop(daemon)
        raise


def post(port: int, message: dict, session: str | None) -> tuple[str | None, dict]:
    headers = {
        "Host": f"127.0.0.1:{port}", "Authorization": f"Bearer {TOKEN}",
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    try:
        conn.request("POST", "/mcp", body=json.dumps(message), headers=headers)
        answer = conn.getresponse()
        body = answer.read()
        if answer.status not in (200, 202):
            raise Failure(f"{message.get('method')} answered {answer.status}: {body[:200]!r}")
        return answer.getheader("Mcp-Session-Id"), (json.loads(body) if body else {})
    finally:
        conn.close()


def calls(port: int) -> int:
    """``CALLS`` sessions of the five tools; the number of requests."""
    requests = 0
    tools = [
        ("hive_create", {"title": "Synthetic", "goal": "G", "acceptance": "true", "step": 1,
                         "source": "manual"}),
        ("hive_list", {"limit": 200}),
        ("hive_show", {"card_id": "h_0000000a"}),
        ("hive_steer", {"card_id": "h_0000000a", "note": "Synthetic note."}),
        ("hive_cancel", {"card_id": "h_0000000a", "reason": "Synthetic."}),
    ]
    for n in range(CALLS):
        session, _ = post(port, {"jsonrpc": "2.0", "id": 1, "method": "initialize",
                                 "params": {"protocolVersion": "2025-11-25", "capabilities": {}}}, None)
        post(port, {"jsonrpc": "2.0", "method": "notifications/initialized"}, session)
        post(port, {"jsonrpc": "2.0", "id": 2, "method": "tools/list"}, session)
        requests += 3
        for i, (name, arguments) in enumerate(tools):
            _, answer = post(port, {"jsonrpc": "2.0", "id": 3 + i, "method": "tools/call",
                                    "params": {"name": name, "arguments": arguments}}, session)
            if answer["result"].get("isError") is not False:
                raise Failure(f"{name} in session {n}: {answer['result']}")
            requests += 1
    return requests


def run(server: Path, work: Path) -> dict[str, int]:
    config = bed(work)
    samples = []
    for _ in range(READY_RESTARTS):
        daemon, ready, _ = spawn(server, config)
        stop(daemon)
        samples.append(ready)
    daemon, _, port = spawn(server, config)
    try:
        time.sleep(SETTLE_SECONDS)
        before = thread_switches(daemon.pid)
        time.sleep(IDLE_SECONDS)
        after = thread_switches(daemon.pid)
        alive(daemon, "while idle")
        requests = calls(port)
        alive(daemon, "after the calls")
        hwm = proc_field(daemon.pid, "status", "VmHWM")
    finally:
        stop(daemon)
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    return {
        "ready_us": p50(samples),
        "ready_restarts": READY_RESTARTS,
        "idle_wakeups": sum(after.values()) - sum(before.values()),
        "idle_threads": len(after),
        "idle_seconds": IDLE_SECONDS,
        "requests": requests,
        "vmhwm_kb": hwm,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: hive_mcp_budget.py ULEI_HIVE_MCP WORKDIR", file=sys.stderr)
        return 2
    try:
        results = run(Path(argv[0]).resolve(), Path(argv[1]).resolve())
    except (Failure, OSError, KeyError, ValueError, subprocess.SubprocessError) as e:
        print(f"hive_mcp_budget: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
