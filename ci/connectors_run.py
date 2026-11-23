"""Runs of the source connectors in their transient units (ADR-0056 sections 5.1 and 5.7).

``python3 ci/connectors_run.py BINDIR WORKDIR`` needs a running user manager reachable over the
user bus, which ``systemd-run --wait`` uses, and ``ulei-connectors`` and ``ulei-connector-test``
in BINDIR. It prints the kernel, the LSM list and the Landlock ABI as a notice, serves a fake C10
socket under WORKDIR that answers each `put` with `stored` or `duplicate`, writes a synthetic
``ulei.toml`` with three connectors and checks: a run lands its items in the fake C10; the test
connector's probes see `socket(AF_INET)` and `socket(AF_UNIX)` refused with `EAFNOSUPPORT` or
`EPERM`; a connector sleeping past its timeout ends `timeout` with its stored cursor kept; and
``ulei-connectors exec`` started outside any unit is refused before it executes anything.

Each check is one GitHub annotation with the measured value; any failed check makes the exit
status 1.
"""

from __future__ import annotations

import ctypes
import errno
import json
import os
import platform
import shutil
import socket
import subprocess
import sys
import threading
from pathlib import Path

from inbox_load import Failure, read_message
from units_run import annotate, check

ITEM_ID = "a" * 64
RUN_TIMEOUT = 120
SLEEPER_TIMEOUT = 2
# The number of landlock_create_ruleset on every architecture, and its VERSION flag.
SYS_LANDLOCK_CREATE_RULESET = 444
LANDLOCK_CREATE_RULESET_VERSION = 1
REFUSED = ("EAFNOSUPPORT errno 97", "EPERM errno 1")


class FakeArchive:
    """A C10 socket that answers `put` and keeps each body."""

    def __init__(self, path: Path) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(8)
        self.puts: list[dict] = []
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
                conn.settimeout(10)
                try:
                    self.answer(conn)
                except (Failure, OSError, ValueError, KeyError) as e:
                    with self.lock:
                        self.errors.append(f"fake C10: {e!r}")

    def answer(self, conn: socket.socket) -> None:
        head, body = read_message(conn, "put")
        line = head.split(b"\r\n", 1)[0]
        if not line.startswith(b"POST /v1/put "):
            raise Failure(f"fake C10 got {line.decode(errors='replace')}")
        put = json.loads(body)
        with self.lock:
            seen = any(
                (p["source"], p["external_id"]) == (put["source"], put["external_id"])
                for p in self.puts
            )
            self.puts.append(put)
        reply = json.dumps({"id": ITEM_ID, "status": "duplicate" if seen else "stored"}).encode()
        conn.sendall(
            f"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n"
            f"Content-Length: {len(reply)}\r\nConnection: close\r\n\r\n".encode() + reply
        )

    def of(self, source: str) -> list[dict]:
        with self.lock:
            if self.errors:
                raise Failure("; ".join(self.errors))
            return [p for p in self.puts if p["source"] == source]

    def close(self) -> None:
        self.listener.close()


def landlock_abi() -> str:
    """The ABI `landlock_create_ruleset(NULL, 0, VERSION)` answers, or its errno."""
    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(
        SYS_LANDLOCK_CREATE_RULESET, None, ctypes.c_size_t(0),
        ctypes.c_uint32(LANDLOCK_CREATE_RULESET_VERSION),
    )
    if abi < 0:
        code = ctypes.get_errno()
        return f"unavailable ({errno.errorcode.get(code, code)})"
    return str(abi)


def kernel() -> str:
    lsm = Path("/sys/kernel/security/lsm")
    lsms = lsm.read_text(encoding="ascii").strip() if lsm.exists() else "absent"
    return f"uname -r {platform.release()}; {lsm} {lsms}; Landlock ABI {landlock_abi()}"


class Bed:
    """The synthetic ``ulei.toml``, connectors, state and log under WORKDIR."""

    def __init__(self, bindir: Path, work: Path) -> None:
        self.runner = bindir / "ulei-connectors"
        self.test = bindir / "ulei-connector-test"
        self.work = work
        self.dir = work / "connectors"
        self.state = work / "state"
        self.log = work / "connectors.log"
        self.c10 = work / "c10.sock"
        self.config = work / "ulei.toml"

    def connector(self, name: str, command: list[str], timeout: int | None = None) -> Path:
        directory = self.dir / name
        (directory / "bin").mkdir(parents=True)
        manifest = {
            "name": name, "version": "0.1.0", "contract": "0.1.0", "transport": "stdio",
            "command": command, "schedule": {"every": 60}, "source": {"addresses": []},
        }
        if timeout is not None:
            manifest["timeout"] = timeout
        (directory / "connector.json").write_text(json.dumps(manifest), encoding="utf-8")
        return directory

    def write(self, enabled: list[str]) -> None:
        self.config.write_text(
            f'[memory]\nsocket = "{self.c10}"\n\n[connectors]\ndir = "{self.dir}"\n'
            f'state = "{self.state}"\nlog = "{self.log}"\nenabled = "{",".join(enabled)}"\n',
            encoding="utf-8",
        )

    def run(self, name: str) -> tuple[subprocess.CompletedProcess, dict]:
        """``ulei-connectors run NAME`` and its run record."""
        done = subprocess.run(
            [str(self.runner), "run", name, "--config", str(self.config)],
            capture_output=True, text=True, check=False, timeout=RUN_TIMEOUT,
        )
        records = [
            json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()
        ] if self.log.exists() else []
        mine = [r for r in records if r["connector"] == name]
        if not mine:
            raise Failure(f"no run record of {name}; exit {done.returncode}: {done.stderr.strip()}")
        return done, mine[-1]

    def cursor(self, name: str) -> str | None:
        state = self.state / name / "runner.json"
        if not state.exists():
            return None
        return json.loads(state.read_text(encoding="utf-8")).get("cursor")


def lands(bed: Bed, archive: FakeArchive) -> str:
    done, record = bed.run("test")
    puts = archive.of("test")
    if done.returncode != 0 or record["outcome"] != "ok":
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    ids = [p["external_id"] for p in puts]
    if ids != [f"item-{n}" for n in range(1, 101)]:
        raise Failure(f"the fake C10 got {len(ids)} puts: {ids[:3]}...")
    unit = record.get("unit", "")
    if not unit.startswith("ulei-connector-test-"):
        raise Failure(f"run record unit {unit!r}")
    return (
        f"exit 0, outcome ok, {len(puts)} puts with source test, cursor {bed.cursor('test')}, "
        f"unit {unit}, {record['duration_ms']} ms; the launcher found NoNewPrivs 1, Seccomp 2 "
        f"and its unit's cgroup"
    )


def probes(bed: Bed, archive: FakeArchive) -> str:
    done, record = bed.run("probe")
    if done.returncode != 0 or record["outcome"] != "ok":
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    answers = {p["external_id"]: p["text"] for p in archive.of("probe")
               if p["external_id"].startswith("probe-")}
    if set(answers) != {"probe-inet", "probe-unix"}:
        raise Failure(f"probe items {sorted(answers)}")
    allowed = [text for text in answers.values() if not text.endswith(REFUSED)]
    if allowed:
        raise Failure(f"not refused with EAFNOSUPPORT or EPERM: {'; '.join(allowed)}")
    return f"{answers['probe-inet']}; {answers['probe-unix']}; unit {record.get('unit')}"


def timeout(bed: Bed, archive: FakeArchive) -> str:
    done, record = bed.run("sleeper")
    cursor = bed.cursor("sleeper")
    ids = [p["external_id"] for p in archive.of("sleeper")]
    if record["outcome"] != "timeout" or done.returncode != 1:
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    if cursor != "c1" or ids != ["s-1"]:
        raise Failure(f"cursor {cursor!r}, puts {ids}; expected c1 and s-1")
    return (
        f"outcome timeout after {record['duration_ms']} ms with timeout {SLEEPER_TIMEOUT} s, "
        f"stored cursor {cursor} kept, puts {ids}, unit {record.get('unit')}"
    )


def outside(bed: Bed) -> str:
    marker = bed.work / "marker"
    data = bed.work / "outside-data"
    done = subprocess.run(
        [str(bed.runner), "exec", "test", str(data), "--", "/usr/bin/touch", str(marker)],
        capture_output=True, text=True, check=False, timeout=30,
    )
    line = done.stderr.strip()
    if done.returncode != 125 or not line.startswith("ulei-connectors: exec refused "):
        raise Failure(f"exit {done.returncode}: {line}")
    if marker.exists() or data.exists():
        raise Failure(f"exit 125 but {marker} or {data} exists: {line}")
    return f"exit 125, {marker.name} not created: {line}"


def run(bindir: Path, work: Path) -> bool:
    annotate("notice", "kernel", kernel())
    bus = Path(os.environ["XDG_RUNTIME_DIR"]) / "bus"
    if not bus.is_socket():
        raise Failure(f"no user bus at {bus}; systemd-run --wait needs it")
    bed = Bed(bindir, work)
    test = bed.connector("test", ["bin/ulei-connector-test"])
    shutil.copy2(bed.test, test / "bin" / "ulei-connector-test")
    probe = bed.connector(
        "probe", ["bin/ulei-connector-test", "--probe-inet", "--probe-unix", str(bed.c10)]
    )
    shutil.copy2(bed.test, probe / "bin" / "ulei-connector-test")
    sleeper = bed.connector("sleeper", ["bin/run.sh"], timeout=SLEEPER_TIMEOUT)
    script = sleeper / "bin" / "run.sh"
    script.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' "
        "'{\"type\":\"item\",\"external_id\":\"s-1\",\"kind\":\"text/plain\","
        "\"text\":\"t\",\"bytes_b64\":\"\"}' '{\"type\":\"checkpoint\",\"cursor\":\"c1\"}'\n"
        "exec sleep 60\n",
        encoding="utf-8",
    )
    script.chmod(0o755)
    bed.write(["test", "probe", "sleeper"])
    archive = FakeArchive(bed.c10)
    try:
        passed = check("connector run lands items in C10", lambda: lands(bed, archive))
        passed &= check("connector sockets refused", lambda: probes(bed, archive))
        passed &= check("connector timeout", lambda: timeout(bed, archive))
        passed &= check("connector exec outside a unit", lambda: outside(bed))
        return passed
    finally:
        archive.close()


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: connectors_run.py BINDIR WORKDIR", file=sys.stderr)
        return 2
    try:
        passed = run(Path(argv[0]).resolve(), Path(argv[1]).resolve())
    except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
        annotate("error", "connector runs", f"setup failed: {e}")
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
