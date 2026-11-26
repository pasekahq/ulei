"""Load client of the `ulei-inbox` budgets in ``ci/budgets.sh`` (ADR-0007 section 4).

``python3 ci/inbox_load.py ULEI_INBOX WORKDIR`` spawns ``ULEI_INBOX serve`` on a synthetic
``ulei.toml`` with a root and socket under WORKDIR, drives it over its Unix socket and stops it;
then it spawns ``serve`` with ``inbox.deliver`` on an enabled root with nothing queued against a
fake engine socket and measures its idle (ADR-0036 section 9). It prints one ``<name> <value>`` line per measurement.
Any non-200 answer or wrong body is a failure.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import socket
import subprocess
import sys
import time
from pathlib import Path

CYCLES = 10_000
IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
READY_RESTARTS = 21
FIRST_STARTS = 5
READY_TIMEOUT = 5.0
REQUEST_TIMEOUT = 30.0
VECTORS = Path(__file__).resolve().parent.parent / "contracts/fixtures/c1/golden/vectors.json"


class Failure(Exception):
    """A run that cannot yield valid budget numbers."""


def canonical(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def sign(packet: dict, key: bytes) -> dict:
    """``packet`` with the `mac` of ADR-0007 section 5 rule 5 under ``key``."""
    mac = hmac.new(key, canonical(packet), hashlib.sha256).hexdigest()
    return {**packet, "mac": mac}


def check_signing() -> None:
    """Signs the first golden vector; a canonical form that differs from Rust fails here."""
    golden = json.loads(VECTORS.read_text(encoding="utf-8"))
    vector = golden["vectors"][0]
    signed = sign(vector["packet"], bytes.fromhex(golden["key_hex"]))
    if canonical(vector["packet"]).hex() != vector["canonical_hex"]:
        raise Failure(f"canonical form of golden vector {vector['name']} differs")
    if signed["mac"] != vector["mac"]:
        raise Failure(f"mac of golden vector {vector['name']} differs")


def call(
    path: str, op: str, body: dict, timeout: float | None = REQUEST_TIMEOUT
) -> tuple[dict, float]:
    """One request on its own connection: the 200 body and the seconds from connect to the
    last response byte. ``timeout`` bounds each socket operation; None waits without bound."""
    data = canonical(body)
    request = (
        f"POST /v1/{op} HTTP/1.1\r\nContent-Type: application/json\r\n"
        f"Content-Length: {len(data)}\r\n\r\n"
    ).encode() + data
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(timeout)
        start = time.perf_counter()
        conn.connect(path)
        conn.sendall(request)
        head, payload = read_message(conn, op)
        elapsed = time.perf_counter() - start
    status = head.split(b"\r\n", 1)[0]
    if not status.startswith(b"HTTP/1.1 200 "):
        raise Failure(f"{op}: {status.decode(errors='replace')} {payload.decode(errors='replace')}")
    return json.loads(payload), elapsed


def read_message(conn: socket.socket, op: str) -> tuple[bytes, bytes]:
    """The head without its empty line and the body of one HTTP message framed by
    Content-Length on ``conn``."""
    received = b""
    length = None
    while length is None or len(received) < length:
        chunk = conn.recv(65536)
        if not chunk:
            raise Failure(f"{op}: connection closed after {len(received)} bytes")
        received += chunk
        if length is None and b"\r\n\r\n" in received:
            head, _ = received.split(b"\r\n\r\n", 1)
            length = len(head) + 4 + content_length(head)
    head, payload = received.split(b"\r\n\r\n", 1)
    return head, payload


def content_length(head: bytes) -> int:
    for line in head.split(b"\r\n")[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            return int(value.strip())
    raise Failure("message without Content-Length")


def write_config(root: Path, path: str, engine: str | None) -> Path:
    """Writes the C9 document ``<path>.toml`` that serves ``root`` on the socket ``path``,
    delivering to the C6 socket ``engine`` when given, and returns it."""
    config = Path(path).with_suffix(".toml")
    text = f'[inbox]\nroot = "{root}"\nsocket = "{path}"\n'
    if engine is not None:
        text += f'deliver = true\n\n[engine]\nc6_socket = "{engine}"\n'
    config.write_text(text, encoding="utf-8")
    return config


def spawn(
    inbox: str, root: Path, path: str, engine: str | None = None, extra: tuple[str, ...] = ()
) -> tuple[subprocess.Popen, float]:
    """A running ``serve`` of the config of :func:`write_config` with the arguments ``extra`` and
    the seconds from spawn to the first 200 of `health`."""
    config = write_config(root, path, engine)
    spawned = time.perf_counter()
    daemon = subprocess.Popen(
        [inbox, "serve", "--config", str(config), *extra], stdout=subprocess.DEVNULL
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


def stop(daemon: subprocess.Popen) -> None:
    daemon.kill()
    daemon.wait()


def median_ready(inbox: str, roots: list[Path], path: str) -> int:
    """Median microseconds from spawn to ready over one start per entry of ``roots``."""
    samples = []
    for root in roots:
        daemon, ready = spawn(inbox, root, path)
        stop(daemon)
        samples.append(ready)
    samples.sort()
    return int(samples[len(samples) // 2] * 1e6)


def proc_field(pid: int, name: str, field: str) -> int:
    for line in Path(f"/proc/{pid}/{name}").read_text(encoding="ascii").splitlines():
        key, _, value = line.partition(":")
        if key == field:
            return int(value.split()[0])
    raise Failure(f"/proc/{pid}/{name} has no {field}")


def cycle(path: str, key: bytes, index: int) -> float:
    """Publish, claim, start and ack of one new event; returns the publish round trip."""
    packet = sign(
        {
            "version": 1,
            "kind": "task_result",
            "id": f"budget:{index:05d}",
            "payload": f"Budget event {index}.",
            "source_ts": int(time.time()),
            "trust": "untrusted-source-data",
        },
        key,
    )
    published, elapsed = call(path, "publish", {"packet": packet})
    if published != {"id": packet["id"]}:
        raise Failure(f"publish answered {published}")
    claimed, _ = call(path, "claim", {})
    row = claimed.get("row")
    if row is None or row["id"] != packet["id"]:
        raise Failure(f"claim of {packet['id']} answered {claimed}")
    lease = {"id": packet["id"], "lease": row["lease"]}
    for op, extra in (("start", {"session_id": "budget"}), ("ack", {"outcome": "success"})):
        applied, _ = call(path, op, {**lease, **extra})
        if applied != {"ok": True}:
            raise Failure(f"{op} of {packet['id']} answered {applied}")
    return elapsed


def measure(inbox: str, work: Path) -> dict[str, int | str]:
    path = str(work / "inbox.sock")
    fresh = [work / f"fresh-{i}" for i in range(FIRST_STARTS)]
    first_start = median_ready(inbox, fresh, path)
    ready = median_ready(inbox, [fresh[0]] * READY_RESTARTS, path)
    root = work / "root"
    daemon, _ = spawn(inbox, root, path)
    try:
        key = (root / "producer.key").read_bytes()
        pid = daemon.pid
        wchar = proc_field(pid, "io", "wchar")
        samples = sorted(cycle(path, key, i) for i in range(CYCLES))
        wchar = proc_field(pid, "io", "wchar") - wchar
        hwm = proc_field(pid, "status", "VmHWM")
        if daemon.poll() is not None:
            raise Failure(f"daemon exited with status {daemon.returncode} during the run")
    finally:
        stop(daemon)
    switches, threads, deliver_hwm = idle_deliver(inbox, work, path)
    return {
        "ready_us": ready,
        "ready_restarts": READY_RESTARTS,
        "ready_root": str(fresh[0]),
        "first_start_us": first_start,
        "first_starts": FIRST_STARTS,
        "cycles": CYCLES,
        "vmhwm_kb": hwm,
        "write_bytes_per_event": wchar // CYCLES,
        "publish_p50_us": int(samples[CYCLES // 2 - 1] * 1e6),
        "publish_p99_us": int(samples[CYCLES * 99 // 100 - 1] * 1e6),
        "idle_wakeups": switches,
        "idle_threads": threads,
        "deliver_vmhwm_kb": deliver_hwm,
        "idle_seconds": IDLE_SECONDS,
    }


def thread_switches(pid: int) -> dict[int, int]:
    """Voluntary context switches of each thread of process ``pid``."""
    return {
        int(task.name): proc_field(pid, f"task/{task.name}/status", "voluntary_ctxt_switches")
        for task in Path(f"/proc/{pid}/task").iterdir()
    }


def idle_deliver(inbox: str, work: Path, path: str) -> tuple[int, int, int]:
    """Voluntary context switches of all threads of ``serve`` with ``inbox.deliver`` over
    ``IDLE_SECONDS`` on an enabled root with nothing queued, the number of threads and the VmHWM
    at the end. The fake engine socket accepts nothing: any connection the loop makes fails the
    run."""
    root = work / "deliver"
    root.mkdir(mode=0o700)
    (root / "ENABLED").touch(mode=0o600)
    engine_path = str(work / "engine.sock")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as engine:
        engine.bind(engine_path)
        engine.listen(1)
        daemon, _ = spawn(inbox, root, path, engine_path)
        try:
            time.sleep(SETTLE_SECONDS)
            before = thread_switches(daemon.pid)
            time.sleep(IDLE_SECONDS)
            after = thread_switches(daemon.pid)
            if daemon.poll() is not None:
                raise Failure(f"daemon exited with status {daemon.returncode} while idle")
            hwm = proc_field(daemon.pid, "status", "VmHWM")
        finally:
            stop(daemon)
        engine.setblocking(False)
        try:
            engine.accept()[0].close()
        except BlockingIOError:
            pass
        else:
            raise Failure("the deliver loop called the engine while idle")
    if before.keys() != after.keys():
        raise Failure(f"threads changed while idle: {sorted(before)} -> {sorted(after)}")
    if len(after) != 3:
        raise Failure(f"serve with inbox.deliver runs {len(after)} threads, expected the server, "
                      "deliver and watch (ADR-0084)")
    return sum(after.values()) - sum(before.values()), len(after), hwm


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: inbox_load.py ULEI_INBOX WORKDIR", file=sys.stderr)
        return 2
    try:
        check_signing()
        results = measure(argv[0], Path(argv[1]))
    except (Failure, OSError) as e:
        print(f"inbox_load: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
