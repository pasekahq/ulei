"""Runs of the source connectors in their transient units (ADR-0056 sections 5 and 5.7).

``python3 ci/connectors_run.py GROUP BINDIR WORKDIR`` needs a running user manager reachable over
the user bus, which ``systemd-run --wait`` uses, and ``ulei-connectors`` and
``ulei-connector-test`` in BINDIR. Each GROUP runs in its own CI step: ``sandbox`` (items land in a fake C10, the
socket probes, the Landlock file probes and grants, the injected ``ENOSYS`` and ``EOPNOTSUPP``,
``landlock = "off"``), ``outcomes`` (``exec_failed``, a unit that cannot start, a non-zero exit, a
timeout with its cursor kept, ``exec`` outside a unit) and ``broker`` (a listed loopback source
under an owner ``allow`` reached with its first line, an unlisted entry and a loopback name without
``allow`` refused ``not_allowed``, the 33rd connection ``limit``, and ``network = "direct"``: its
own ``AF_INET`` socket, ``AF_UNIX`` still refused, connect ports limited from Landlock ABI 4) and
``schedule`` (``ulei-connectors serve`` for about two periods of 60 s: each connector once per
period and an overdue one once at start, ``rate_limited`` putting a run off across a restart,
``sync`` called, no voluntary context switch between due times, a manual ``run`` during a
scheduled one ``busy``, and SIGTERM stopping the running unit with its record written).
Every group first reads the kernel, the LSM list and the Landlock ABI and fails without Landlock
ABI 3 or later, and last checks that no broker socket is left in ``run_dir``.

GitHub keeps at most 50 annotations per job and 10 notices per step, so a group folds its passed
checks into two to four notices, one line per check with its title and measured value. A failed
check is an error annotation of its own; any failed check makes the exit status 1.
"""

from __future__ import annotations

import ctypes
import errno
import json
import os
import platform
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from annotations import Notices, annotate
from inbox_load import Failure, read_message

ITEM_ID = "a" * 64
RUN_TIMEOUT = 120
SLEEPER_TIMEOUT = 2
# The number of landlock_create_ruleset on every architecture, and its VERSION flag.
SYS_LANDLOCK_CREATE_RULESET = 444
LANDLOCK_CREATE_RULESET_VERSION = 1
# Below it truncation is never checked (ADR-0056 section 5.2).
MIN_ABI = 3
REFUSED = ("EAFNOSUPPORT errno 97", "EPERM errno 1")
GROUPS = ("sandbox", "outcomes", "broker", "schedule")
# The schedule group: every connector's period and the sync period, the retry_after its
# rate-limited connector asks for, and when the driver acts, in seconds after `serve` starts.
PERIOD = 60
RETRY_AFTER = 75
MANUAL_AT = 5
IDLE_FROM, IDLE_TO = 10, 50
SIGTERM_AT = 130
RESTART_FOR = 5
# How far a run may start from its due time.
SLACK = 3.0
# The connections a run gets from its broker (ADR-0056 section 5.3).
MAX_CONNECTIONS = 32
# The first Landlock ABI with TCP port rules.
NET_ABI = 4
SOURCE_LINE = b"fake source line\n"
ACCESS = "EACCES errno 13"
ARCHIVE_BYTES = b"synthetic archive bytes\n" * 32
# Inserts the test unit's filter into the runner's systemd-run argv, after the runner's own.
SHIM = """#!/usr/bin/python3
import os, sys
args = sys.argv[1:]
cut = args.index("--")
extra = ["--property=SystemCallFilter=~landlock_create_ruleset",
         "--property=SystemCallErrorNumber={errno}"]
os.execv({real!r}, ["systemd-run", *args[:cut], *extra, *args[cut:]])
"""


class FakeArchive:
    """A C10 socket that answers `put` and keeps each body."""

    def __init__(self, path: Path) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen(8)
        self.puts: list[dict] = []
        self.keys: set[tuple[str, str]] = set()
        self.syncs: list[float] = []
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
        if line.startswith(b"POST /v1/sync "):
            with self.lock:
                self.syncs.append(time.time())
            self.reply(conn, {"put": 0, "synced": True})
            return
        if not line.startswith(b"POST /v1/put "):
            raise Failure(f"fake C10 got {line.decode(errors='replace')}")
        put = json.loads(body)
        key = (put["source"], put["external_id"])
        with self.lock:
            seen = key in self.keys
            self.keys.add(key)
            self.puts.append(put)
        self.reply(conn, {"id": ITEM_ID, "status": "duplicate" if seen else "stored"})

    def reply(self, conn: socket.socket, answer: dict) -> None:
        reply = json.dumps(answer).encode()
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


class FakeSource:
    """A loopback TCP source that sends one line on each connection and closes it."""

    def __init__(self) -> None:
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(64)
        self.port = self.listener.getsockname()[1]
        self.accepted = 0
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            self.accepted += 1
            with conn:
                try:
                    conn.sendall(SOURCE_LINE)
                except OSError:
                    pass

    def close(self) -> None:
        self.listener.close()


def landlock_abi() -> tuple[int | None, str]:
    """The ABI `landlock_create_ruleset(NULL, 0, VERSION)` answers, none on an errno, and the
    answer as text."""
    libc = ctypes.CDLL(None, use_errno=True)
    abi = libc.syscall(
        SYS_LANDLOCK_CREATE_RULESET, None, ctypes.c_size_t(0),
        ctypes.c_uint32(LANDLOCK_CREATE_RULESET_VERSION),
    )
    if abi < 0:
        code = ctypes.get_errno()
        return None, f"unavailable ({errno.errorcode.get(code, code)})"
    return abi, str(abi)


def kernel() -> tuple[int | None, str]:
    """The Landlock ABI and the notice line of the kernel."""
    lsm = Path("/sys/kernel/security/lsm")
    lsms = lsm.read_text(encoding="ascii").strip() if lsm.exists() else "absent"
    abi, text = landlock_abi()
    return abi, f"uname -r {platform.release()}; {lsm} {lsms}; Landlock ABI {text}"


class Bed:
    """The synthetic ``ulei.toml``, connectors, state and log under WORKDIR."""

    def __init__(self, bindir: Path, work: Path) -> None:
        self.runner = bindir / "ulei-connectors"
        self.test = bindir / "ulei-connector-test"
        self.work = work
        self.dir = work / "connectors"
        self.state = work / "state"
        self.log = work / "connectors.log"
        self.run_dir = work / "run"
        self.c10 = work / "c10.sock"
        self.config = work / "ulei.toml"

    def connector(
        self, name: str, command: list[str], timeout: int | None = None, key: str | None = None,
        addresses: list[str] | None = None, every: int = 60,
    ) -> Path:
        directory = self.dir / name
        (directory / "bin").mkdir(parents=True)
        manifest = {
            "name": name, "version": "0.1.0", "contract": "0.1.0", "transport": "stdio",
            "command": command, "schedule": {"every": every},
            "source": {"addresses": addresses or []},
        }
        if timeout is not None:
            manifest["timeout"] = timeout
        if key is not None:
            manifest["key"] = key
        (directory / "connector.json").write_text(json.dumps(manifest), encoding="utf-8")
        return directory

    def write(
        self, enabled: list[str], owner: dict[str, dict[str, str]], sync_every: int | None = None,
    ) -> None:
        """``ulei.toml`` with ``enabled``, ``sync_every`` when given and the
        ``[connectors.<name>]`` records of ``owner``."""
        text = (
            f'[memory]\nsocket = "{self.c10}"\n\n[connectors]\ndir = "{self.dir}"\n'
            f'state = "{self.state}"\nlog = "{self.log}"\nrun_dir = "{self.run_dir}"\n'
            f'enabled = "{",".join(enabled)}"\n'
        )
        if sync_every is not None:
            text += f"sync_every = {sync_every}\n"
        for name, keys in owner.items():
            text += f"\n[connectors.{name}]\n" + "".join(
                f'{key} = "{value}"\n' for key, value in keys.items()
            )
        self.config.write_text(text, encoding="utf-8")

    def run(
        self, name: str, env: dict[str, str] | None = None,
    ) -> tuple[subprocess.CompletedProcess, dict]:
        """``ulei-connectors run NAME`` under ``env`` and its run record."""
        done = subprocess.run(
            [str(self.runner), "run", name, "--config", str(self.config)],
            capture_output=True, text=True, check=False, timeout=RUN_TIMEOUT,
            env=env,
        )
        records = [
            json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()
        ] if self.log.exists() else []
        mine = [r for r in records if r["connector"] == name]
        if not mine:
            raise Failure(f"no run record of {name}; exit {done.returncode}: {done.stderr.strip()}")
        return done, mine[-1]

    def records(self) -> list[dict]:
        return run_records(self.log)

    def cursor(self, name: str) -> str | None:
        state = self.state / name / "runner.json"
        if not state.exists():
            return None
        return json.loads(state.read_text(encoding="utf-8")).get("cursor")


def run_records(log: Path) -> list[dict]:
    """The run records of ``log``; a line still being appended is left out."""
    if not log.exists():
        return []
    lines = log.read_text(encoding="utf-8").splitlines(keepends=True)
    return [json.loads(line) for line in lines if line.endswith("\n")]


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
    abi = record.get("landlock_abi")
    if not isinstance(abi, int) or abi < MIN_ABI:
        raise Failure(f"run record landlock_abi {abi!r}")
    if f"landlock abi {abi}" not in done.stderr:
        raise Failure(f"no landlock abi {abi} in the run's line: {done.stderr.strip()}")
    return (
        f"exit 0, outcome ok, {len(puts)} puts with source test, cursor {bed.cursor('test')}, "
        f"unit {unit}, {record['duration_ms']} ms, landlock_abi {abi}; the launcher found "
        f"NoNewPrivs 1, Seccomp 2 and its unit's cgroup"
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


class Files:
    """The paths of the file probes: the run's own, the owner's grants and what lies outside."""

    def __init__(self, bed: Bed, name: str) -> None:
        work = bed.work
        self.name = name
        self.own = bed.dir / name
        self.data = bed.state / name / "data"
        self.keys = work / "keys"
        self.key = self.keys / f"{name}.env"
        self.other_key = self.keys / "probe.env"
        self.shared = work / "shared"
        self.outbox = work / "outbox"
        self.outside = work / "outside.txt"
        self.archive = work / "archive" / "archive.bin"
        self.created_outside = work / "created-outside.txt"

    def make(self) -> None:
        for directory in (self.keys, self.shared, self.outbox, self.archive.parent):
            directory.mkdir(parents=True, exist_ok=True)
        self.key.write_text("FILES_TOKEN=synthetic\n", encoding="utf-8")
        self.other_key.write_text("PROBE_TOKEN=synthetic\n", encoding="utf-8")
        (self.shared / "shared.txt").write_text("shared\n", encoding="utf-8")
        self.outside.write_text("outside\n", encoding="utf-8")
        self.archive.write_bytes(ARCHIVE_BYTES)

    def owner(self) -> dict[str, str]:
        return {"key_file": str(self.key), "read": str(self.shared), "write": str(self.outbox)}

    def allowed(self) -> list[tuple[str, Path]]:
        """Calls inside the run's paths and grants, in order."""
        return [
            ("read", self.shared / "shared.txt"),
            ("read", self.own / "connector.json"),
            ("create", self.data / "created.txt"),
            ("write", self.data / "created.txt"),
            ("truncate", self.data / "created.txt"),
            ("create", self.outbox / "out.txt"),
            ("write", self.outbox / "out.txt"),
        ]

    def refused(self, config: Path) -> list[tuple[str, Path]]:
        """Calls outside them, in order: each must answer `EACCES`."""
        return [
            ("read", self.outside),
            ("read", self.other_key),
            ("read", config),
            ("write", self.outside),
            ("create", self.created_outside),
            ("create", self.own / "created.txt"),
            ("truncate", self.outside),
            ("truncate", self.archive),
            ("write", self.shared / "shared.txt"),
        ]

    def command(self, config: Path) -> list[str]:
        args = ["bin/ulei-connector-test"]
        for call, path in self.allowed() + self.refused(config):
            args += [f"--probe-{call}", str(path)]
        return args


def files(bed: Bed, archive: FakeArchive, paths: Files) -> tuple[str, str]:
    """The file probes' run: the refusals' and the allowed calls' annotation texts."""
    done, record = bed.run(paths.name)
    if done.returncode != 0 or record["outcome"] != "ok":
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    puts = archive.of(paths.name)
    answers = [p["text"] for p in puts if p["external_id"].startswith("probe-")]
    allowed, refused = paths.allowed(), paths.refused(bed.config)
    if len(answers) != len(allowed) + len(refused):
        raise Failure(f"{len(answers)} probe items for {len(allowed) + len(refused)} probes")
    wrong = []
    for (call, path), answer in zip(allowed, answers[: len(allowed)]):
        if not answer.startswith(f"{call} {path}: ") or "errno" in answer:
            wrong.append(f"allowed {answer}")
    for (call, path), answer in zip(refused, answers[len(allowed):]):
        if answer != f"{call} {path}: {ACCESS}":
            wrong.append(f"not EACCES: {answer}")
    if paths.archive.read_bytes() != ARCHIVE_BYTES:
        wrong.append(f"{paths.archive} changed to {paths.archive.stat().st_size} bytes")
    if paths.outside.read_text(encoding="utf-8") != "outside\n":
        wrong.append(f"{paths.outside} changed")
    if paths.created_outside.exists() or (paths.own / "created.txt").exists():
        wrong.append("a file was created outside the data directory and the grants")
    keyed = [p for p in puts if p["external_id"].startswith("item-")]
    if not keyed or "key sha256:" not in keyed[0]["text"]:
        wrong.append("the items carry no hash of the connector's own key file")
    if wrong:
        raise Failure("; ".join(wrong))
    abi = record.get("landlock_abi")
    work = str(bed.work)
    refusals = "; ".join(a.replace(work, "WORKDIR") for a in answers[len(allowed):])
    granted = "; ".join(a.replace(work, "WORKDIR") for a in answers[: len(allowed)])
    return (
        f"landlock_abi {abi}, unit {record.get('unit')}: {refusals}; "
        f"{paths.archive.name} {len(ARCHIVE_BYTES)} bytes unchanged",
        f"landlock_abi {abi}, own key file read (its hash in the items), grants "
        f"{json.dumps(record['grants'])}: {granted}",
    )


def shimmed(bed: Bed, errno_name: str) -> dict[str, str]:
    """An environment whose ``systemd-run`` gives the unit a filter answering
    `landlock_create_ruleset` with ``errno_name``."""
    real = shutil.which("systemd-run")
    if real is None:
        raise Failure("no systemd-run on PATH")
    shim = bed.work / f"shim-{errno_name}"
    shim.mkdir(exist_ok=True)
    script = shim / "systemd-run"
    script.write_text(SHIM.format(errno=errno_name, real=real), encoding="utf-8")
    script.chmod(0o755)
    return {**os.environ, "PATH": f"{shim}:{os.environ['PATH']}"}


def injected(bed: Bed, errno_name: str) -> str:
    done, record = bed.run("test", shimmed(bed, errno_name))
    want = (
        f"ulei-connectors: exec refused landlock: landlock unavailable ({errno_name}); "
        "Linux 6.2 or later with landlock in the LSM list is required, or set "
        'connectors.test.landlock = "off"'
    )
    if record["outcome"] != "refused" or record.get("code") != "landlock" or done.returncode != 1:
        raise Failure(
            f"exit {done.returncode}, outcome {record['outcome']} {record.get('code')}: "
            f"{done.stderr.strip()}"
        )
    if want not in record["stderr"] or f"landlock unavailable ({errno_name})" not in done.stderr:
        raise Failure(f"record stderr {record['stderr']!r}; line {done.stderr.strip()}")
    return f"outcome refused, code landlock; log: {want}; journal: {done.stderr.strip()}"


def opted_out(bed: Bed) -> str:
    done, record = bed.run("unconfined", shimmed(bed, "ENOSYS"))
    if done.returncode != 0 or record["outcome"] != "ok":
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    if record["grants"] != {"landlock": "off"} or "landlock_abi" in record:
        raise Failure(f"grants {record['grants']}, landlock_abi {record.get('landlock_abi')}")
    if "landlock off" not in done.stderr:
        raise Failure(f"no landlock off in the run's line: {done.stderr.strip()}")
    return (
        f"under the injected ENOSYS: outcome ok, record grants {json.dumps(record['grants'])}, "
        f"no landlock_abi; journal: {done.stderr.strip()}"
    )


def exec_failed(bed: Bed, names: list[str]) -> str:
    """Each of ``names`` ends `refused` with code `exec_failed` after the launcher's Landlock."""
    shown = []
    for name in names:
        done, record = bed.run(name)
        if (
            record["outcome"] != "refused" or record.get("code") != "exec_failed"
            or done.returncode != 1
        ):
            raise Failure(
                f"{name}: exit {done.returncode}, outcome {record['outcome']} "
                f"{record.get('code')}: {done.stderr.strip()}"
            )
        shown.append(
            f"{name}: outcome refused, code exec_failed, landlock_abi "
            f"{record.get('landlock_abi')}; {done.stderr.strip().replace(str(bed.work), 'WORKDIR')}"
        )
    return "; ".join(shown)


def unit_start(bed: Bed) -> str:
    nobus = bed.work / "nobus"
    nobus.mkdir(exist_ok=True)
    env = {
        **os.environ, "XDG_RUNTIME_DIR": str(nobus),
        "DBUS_SESSION_BUS_ADDRESS": f"unix:path={nobus / 'bus'}",
    }
    done, record = bed.run("test", env)
    if record["outcome"] != "refused" or record.get("code") != "unit_start_failed":
        raise Failure(
            f"exit {done.returncode}, outcome {record['outcome']} {record.get('code')}: "
            f"{done.stderr.strip()}"
        )
    if "unit" in record:
        raise Failure(f"run record names unit {record['unit']} that never started")
    return f"user bus absent: outcome refused, code unit_start_failed; {done.stderr.strip()}"


def timeout(bed: Bed, archive: FakeArchive) -> str:
    done, record = bed.run("sleeper")
    cursor = bed.cursor("sleeper")
    ids = [p["external_id"] for p in archive.of("sleeper")]
    if record["outcome"] != "timeout" or done.returncode != 1:
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    if cursor != "c1" or ids != ["s-1"]:
        raise Failure(f"cursor {cursor!r}, puts {ids}; expected c1 and s-1")
    return (
        f"outcome timeout from the unit's Result=timeout after {record['duration_ms']} ms with "
        f"timeout {SLEEPER_TIMEOUT} s, stored cursor {cursor} kept, puts {ids}, "
        f"unit {record.get('unit')}"
    )


def outside(bed: Bed) -> str:
    marker = bed.work / "marker"
    data = bed.work / "outside-data"
    done = subprocess.run(
        [str(bed.runner), "exec", "test", str(data), "/usr", "--", "/usr/bin/touch", str(marker)],
        capture_output=True, text=True, check=False, timeout=30,
    )
    line = done.stderr.strip()
    if done.returncode != 125 or not line.startswith("ulei-connectors: exec refused "):
        raise Failure(f"exit {done.returncode}: {line}")
    if marker.exists() or data.exists():
        raise Failure(f"exit 125 but {marker} or {data} exists: {line}")
    return f"exit 125, {marker.name} not created: {line}"


def exited(bed: Bed) -> str:
    done, record = bed.run("failing")
    if record["outcome"] != "exit" or done.returncode != 1:
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    return f"outcome exit, unit {record.get('unit')}; {done.stderr.strip()}"


def probe_texts(bed: Bed, archive: FakeArchive, name: str) -> tuple[dict, list[str]]:
    """The run record of an `ok` run of ``name`` and the texts of its probe items, in order."""
    done, record = bed.run(name)
    if done.returncode != 0 or record["outcome"] != "ok":
        raise Failure(f"exit {done.returncode}, outcome {record['outcome']}: {done.stderr.strip()}")
    texts = [p["text"] for p in archive.of(name) if p["external_id"].startswith("probe-")]
    return record, texts


class Sources:
    """The fake TCP sources of the broker group: ``listed`` is named by the manifests, ``other``
    by none."""

    def __init__(self) -> None:
        self.listed = FakeSource()
        self.other = FakeSource()

    def close(self) -> None:
        self.listed.close()
        self.other.close()


def reached(bed: Bed, archive: FakeArchive, sources: Sources) -> tuple[str, str]:
    """The listed source under an owner `allow`, then an unlisted entry, in one run."""
    record, texts = probe_texts(bed, archive, "broker")
    entry = f"localhost:{sources.listed.port}"
    if len(texts) != 2:
        raise Failure(f"probe items {texts}")
    ok_prefix = f'connect {entry}: {{"ok":"'
    ok_suffix = f'"}}; first line: {SOURCE_LINE.decode().strip()}'
    if not (texts[0].startswith(ok_prefix) and texts[0].endswith(ok_suffix)):
        raise Failure(f"listed source: {texts[0]}")
    unlisted = f'connect 127.0.0.1:{sources.other.port}: {{"error":"not_allowed"}}'
    if texts[1] != unlisted:
        raise Failure(f"unlisted entry: {texts[1]}")
    if sources.other.accepted:
        raise Failure(f"the unlisted source accepted {sources.other.accepted} connections")
    grants = json.dumps(record["grants"])
    return (
        f"source.addresses [{entry}], grants {grants}; socket on fd 3 named broker by "
        f"OpenFile=, reply and first line through the SCM_RIGHTS socket: {texts[0]}",
        f"{texts[1]}; the unlisted source accepted 0 connections",
    )


def loopback(bed: Bed, archive: FakeArchive, sources: Sources) -> str:
    record, texts = probe_texts(bed, archive, "loopback")
    entry = f"localhost:{sources.listed.port}"
    want = f'connect {entry}: {{"error":"not_allowed"}}'
    if texts != [want]:
        raise Failure(f"probe items {texts}")
    return f"{entry} listed in source.addresses, grants {json.dumps(record['grants'])}: {want}"


def limited(bed: Bed, archive: FakeArchive, sources: Sources) -> str:
    _, texts = probe_texts(bed, archive, "limit")
    entry = f"localhost:{sources.listed.port}"
    want = f'limit {entry}: {MAX_CONNECTIONS} ok, then {{"error":"limit"}}'
    if texts != [want]:
        raise Failure(f"probe items {texts}")
    return f"{MAX_CONNECTIONS + 1} requests with every passed socket kept open: {want}"


def direct(bed: Bed, archive: FakeArchive, sources: Sources) -> tuple[str, str]:
    """``network = "direct"``: its own sockets, and its connect ports from Landlock ABI 4."""
    record, texts = probe_texts(bed, archive, "direct")
    abi = record.get("landlock_abi")
    if not isinstance(abi, int) or abi < MIN_ABI:
        raise Failure(f"run record landlock_abi {abi!r}")
    listed, other = sources.listed.port, sources.other.port
    if len(texts) != 5:
        raise Failure(f"probe items {texts}")
    inet, unix, to_listed, to_other, brokered = texts
    wrong = []
    if inet != "socket(AF_INET): created":
        wrong.append(inet)
    if not unix.endswith(REFUSED):
        wrong.append(f"not refused: {unix}")
    if not brokered.startswith(f'connect 127.0.0.1:{listed}: {{"ok":"127.0.0.1:{listed}"}}'):
        wrong.append(f"broker under direct: {brokered}")
    if record["grants"].get("network") != "direct":
        wrong.append(f"grants {record['grants']}")
    connected = "socket(AF_INET) created, connected"
    limited_ports = abi >= NET_ABI
    want_other = f"socket(AF_INET) created, connect: {ACCESS}" if limited_ports else connected
    if to_listed != f"direct 127.0.0.1:{listed}: {connected}":
        wrong.append(f"listed port: {to_listed}")
    if to_other != f"direct 127.0.0.1:{other}: {want_other}":
        wrong.append(f"unlisted port: {to_other}")
    want_ports = [listed] if limited_ports else None
    if record.get("connect_ports") != want_ports:
        wrong.append(f"connect_ports {record.get('connect_ports')}, expected {want_ports}")
    if wrong:
        raise Failure("; ".join(wrong))
    work = str(bed.work)
    sockets = (
        f"RestrictAddressFamilies=AF_INET AF_INET6: {inet}; {unix.replace(work, 'WORKDIR')}; "
        f"broker still passed: {brokered}"
    )
    ports = (
        f"landlock_abi {abi}, record connect_ports {record.get('connect_ports')}: {to_listed}; "
        f"{to_other}"
    )
    if not limited_ports:
        ports += (
            f"; below ABI {NET_ABI} the ports are not limited and the record has no connect_ports"
        )
    return sockets, ports


def removed(bed: Bed) -> str:
    left = sorted(p.name for p in bed.run_dir.iterdir()) if bed.run_dir.exists() else []
    records = [
        json.loads(line) for line in bed.log.read_text(encoding="utf-8").splitlines()
    ] if bed.log.exists() else []
    outcomes = sorted({r["outcome"] for r in records})
    if left:
        raise Failure(f"left in run_dir after {len(records)} runs: {left}")
    return f"run_dir empty after {len(records)} runs with outcomes {', '.join(outcomes)}"


# The notices of each group: the first also holds the kernel line, the last the run_dir check.
FOLDS = {
    "sandbox": (
        "connector sandbox: kernel and items",
        "connector sandbox: refusals",
        "connector sandbox: paths, grants, opt-out and run_dir",
    ),
    "outcomes": (
        "connector outcomes: kernel and refusals",
        "connector outcomes: exit, timeout, exec outside a unit and run_dir",
    ),
    "broker": (
        "connector broker: kernel and allow rules",
        "connector broker: direct network and run_dir",
    ),
    "schedule": (
        "connector schedule: kernel, periods and sync",
        "connector schedule: rate_limited and busy",
        "connector schedule: idle",
        "connector schedule: SIGTERM and run_dir",
    ),
}


def setup(bindir: Path, work: Path, sources: Sources) -> tuple[Bed, Files]:
    """The connectors of every group and their ``ulei.toml``."""
    bed = Bed(bindir, work)
    paths = Files(bed, "files")
    paths.make()
    listed = f"localhost:{sources.listed.port}"
    direct_entry = f"127.0.0.1:{sources.listed.port}"
    for name, command, key, addresses in (
        ("test", ["bin/ulei-connector-test"], None, None),
        ("unconfined", ["bin/ulei-connector-test"], None, None),
        ("probe", ["bin/ulei-connector-test", "--probe-inet", "--probe-unix", str(bed.c10)],
         "probe", None),
        (paths.name, paths.command(bed.config), "files", None),
        ("broker", ["bin/ulei-connector-test", "--connect", listed, "--connect",
                    f"127.0.0.1:{sources.other.port}"], None, [listed]),
        ("loopback", ["bin/ulei-connector-test", "--connect", listed], None, [listed]),
        ("limit", ["bin/ulei-connector-test", "--probe-limit", listed], None, [listed]),
        ("direct", ["bin/ulei-connector-test", "--probe-inet", "--probe-unix", str(bed.c10),
                    "--probe-direct", direct_entry,
                    "--probe-direct", f"127.0.0.1:{sources.other.port}",
                    "--connect", direct_entry], None, [direct_entry]),
    ):
        directory = bed.connector(name, command, key=key, addresses=addresses)
        shutil.copy2(bed.test, directory / "bin" / "ulei-connector-test")
    bed.connector("missing", ["bin/missing"])
    elsewhere = work / "elsewhere"
    elsewhere.mkdir()
    shutil.copy2(bed.test, elsewhere / "ulei-connector-test")
    # The manifest takes only paths inside the connector's directory or under /usr; a link
    # there to a program elsewhere reaches execve, where Landlock resolves it.
    outsider = bed.connector("outsider", ["bin/linked"])
    (outsider / "bin" / "linked").symlink_to(elsewhere / "ulei-connector-test")
    failing = bed.connector("failing", ["bin/run.sh"])
    (failing / "bin" / "run.sh").write_text("#!/bin/sh\nexit 3\n", encoding="utf-8")
    (failing / "bin" / "run.sh").chmod(0o755)
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
    bed.write(
        ["test", "unconfined", "probe", paths.name, "missing", "outsider", "failing", "sleeper",
         "broker", "loopback", "limit", "direct"],
        {
            "unconfined": {"landlock": "off"},
            "probe": {"key_file": str(paths.other_key)},
            paths.name: paths.owner(),
            "broker": {"allow": listed},
            "limit": {"allow": listed},
            "direct": {"network": "direct", "allow": direct_entry},
        },
    )
    return bed, paths


def sandbox(notices: Notices, bed: Bed, archive: FakeArchive, paths: Files) -> bool:
    items, refusals, grants = FOLDS["sandbox"]
    passed = notices.check(items, "connector run lands items in C10", lambda: lands(bed, archive))
    passed &= notices.check(refusals, "connector sockets refused", lambda: probes(bed, archive))
    try:
        landlocked, granted = files(bed, archive, paths)
    except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
        annotate("error", "connector Landlock refusals", str(e))
        passed = False
    else:
        notices.add(refusals, "connector Landlock refusals", landlocked)
        notices.add(grants, "connector Landlock paths and grants", granted)
    for errno_name in ("ENOSYS", "EOPNOTSUPP"):
        passed &= notices.check(
            refusals, f"connector refused on injected {errno_name}",
            lambda errno_name=errno_name: injected(bed, errno_name),
        )
    passed &= notices.check(grants, "connector landlock off", lambda: opted_out(bed))
    return passed


def outcomes(notices: Notices, bed: Bed, archive: FakeArchive) -> bool:
    refusals, ends = FOLDS["outcomes"]
    passed = notices.check(
        refusals, "connector exec failed", lambda: exec_failed(bed, ["missing", "outsider"]),
    )
    passed &= notices.check(refusals, "connector unit fails to start", lambda: unit_start(bed))
    passed &= notices.check(ends, "connector exit", lambda: exited(bed))
    passed &= notices.check(ends, "connector timeout", lambda: timeout(bed, archive))
    passed &= notices.check(ends, "connector exec outside a unit", lambda: outside(bed))
    return passed


def broker(notices: Notices, bed: Bed, archive: FakeArchive, sources: Sources) -> bool:
    rules, direct_network = FOLDS["broker"]
    passed = notices.pair(
        rules,
        ("connector broker reaches a listed source", "connector broker refuses an unlisted entry"),
        lambda: reached(bed, archive, sources),
    )
    passed &= notices.check(
        rules, "connector broker refuses a loopback name without allow",
        lambda: loopback(bed, archive, sources),
    )
    passed &= notices.check(
        rules, "connector broker 33rd connection", lambda: limited(bed, archive, sources),
    )
    passed &= notices.pair(
        direct_network, ("connector direct sockets", "connector direct connect ports"),
        lambda: direct(bed, archive, sources),
    )
    return passed


def script(
    bed: Bed, name: str, body: str, timeout: int | None = None, every: int = 60,
) -> None:
    """A ``/bin/sh`` connector ``name`` running ``body``."""
    directory = bed.connector(name, ["bin/run.sh"], timeout=timeout, every=every)
    program = directory / "bin" / "run.sh"
    program.write_text(f"#!/bin/sh\n{body}\n", encoding="utf-8")
    program.chmod(0o755)


def switches(pid: int) -> dict[str, int]:
    """``voluntary_ctxt_switches`` of each thread of ``pid``."""
    counts = {}
    for task in Path(f"/proc/{pid}/task").iterdir():
        for line in (task / "status").read_text(encoding="ascii").splitlines():
            if line.startswith("voluntary_ctxt_switches:"):
                counts[task.name] = int(line.split()[1])
    return counts


def offsets(times: list[float], base: float) -> list[float]:
    return [round(t - base, 1) for t in times]


def every_period(times: list[float], base: float, count: int) -> bool:
    """Whether ``times`` are ``count`` starts one period apart from ``base``."""
    return len(times) == count and all(
        abs(t - base - n * PERIOD) <= SLACK for n, t in enumerate(times)
    )


class Served:
    """The schedule group's measurements of one `serve` of about two periods."""

    def __init__(self, bed: Bed, archive: FakeArchive) -> None:
        self.bed = bed
        self.archive = archive
        self.start = 0.0
        self.manual: tuple[subprocess.CompletedProcess, dict] | None = None
        self.idle: tuple[dict[str, int], dict[str, int]] = ({}, {})
        self.exit: int | None = None
        self.stopped_after = 0.0
        self.stderr = ""
        self.restart_exit: int | None = None
        self.restart_new: list[dict] = []
        self.not_before: float | None = None

    def serve(self, err: Path) -> subprocess.Popen:
        with err.open("w", encoding="utf-8") as sink:
            return subprocess.Popen(
                [str(self.bed.runner), "serve", "--config", str(self.bed.config)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=sink,
            )

    def wait_until(self, offset: float) -> None:
        time.sleep(max(0.0, self.start + offset - time.time()))

    def drive(self) -> None:
        self.start = time.time()
        served = self.serve(self.bed.work / "serve.err")
        try:
            self.wait_until(MANUAL_AT)
            self.manual = self.bed.run("slow")
            self.wait_until(IDLE_FROM)
            first = switches(served.pid)
            self.wait_until(IDLE_TO)
            self.idle = (first, switches(served.pid))
            self.wait_until(SIGTERM_AT)
            sent = time.time()
            served.send_signal(signal.SIGTERM)
            self.exit = served.wait(timeout=60)
            self.stopped_after = time.time() - sent
        finally:
            if served.poll() is None:
                served.kill()
                served.wait()
        self.stderr = (self.bed.work / "serve.err").read_text(encoding="utf-8")
        state = json.loads((self.bed.state / "limited" / "runner.json").read_text("utf-8"))
        self.not_before = state.get("not_before")
        before = len(self.bed.records())
        self.bed.write(["periodic", "overdue", "limited"], {}, sync_every=PERIOD)
        again = self.serve(self.bed.work / "restart.err")
        try:
            time.sleep(RESTART_FOR)
            again.send_signal(signal.SIGTERM)
            self.restart_exit = again.wait(timeout=60)
        finally:
            if again.poll() is None:
                again.kill()
                again.wait()
        self.restart_new = self.bed.records()[before:]

    def of(self, name: str, outcome: str | None = None) -> list[dict]:
        return [
            r for r in self.bed.records()
            if r["connector"] == name and (outcome is None or r["outcome"] == outcome)
        ]


def periods(served: Served) -> str:
    periodic = [r["at"] for r in served.of("periodic") if r["outcome"] != "busy"]
    overdue = [r["at"] for r in served.of("overdue") if r["outcome"] != "busy"]
    base = periodic[0] if periodic else served.start
    wrong = []
    for name, times in (("periodic", periodic), ("overdue", overdue)):
        if not every_period(times, base, 3):
            wrong.append(f"{name} started at {offsets(times, base)} s")
    outcomes = {r["outcome"] for r in served.of("periodic") + served.of("overdue")}
    if outcomes != {"ok"}:
        wrong.append(f"outcomes {sorted(outcomes)}")
    if wrong:
        raise Failure("; ".join(wrong) + f", expected 0, {PERIOD}, {2 * PERIOD} s")
    return (
        f"every {PERIOD} s, SIGTERM at {SIGTERM_AT} s: periodic (never run) started at "
        f"{offsets(periodic, base)} s, overdue (last start {PERIOD * 60} s before) at "
        f"{offsets(overdue, base)} s, once at start; all ok"
    )


def rate_limited(served: Served) -> str:
    runs = [r for r in served.of("limited") if r["outcome"] != "busy"]
    times = [r["at"] for r in runs]
    base = served.of("periodic")[0]["at"] if served.of("periodic") else served.start
    codes = {r.get("code") for r in runs}
    wrong = []
    if len(times) != 2 or abs(times[0] - base) > SLACK:
        wrong.append(f"limited started at {offsets(times, base)} s")
    elif not RETRY_AFTER <= times[1] - times[0] <= RETRY_AFTER + SLACK:
        wrong.append(f"its second run came {times[1] - times[0]:.1f} s after the first")
    if codes != {"rate_limited"}:
        wrong.append(f"codes {sorted(codes, key=str)}")
    if served.not_before is None or served.not_before < times[-1] + RETRY_AFTER:
        wrong.append(f"runner.json not_before {served.not_before}")
    new = served.restart_new
    if served.restart_exit != 0 or new:
        wrong.append(
            f"restart exit {served.restart_exit}, new runs "
            f"{[(r['connector'], r['outcome']) for r in new]}"
        )
    if wrong:
        raise Failure("; ".join(wrong))
    return (
        f"retry_after {RETRY_AFTER} s with period {PERIOD} s: runs at {offsets(times, base)} s, "
        f"none at {PERIOD} s; runner.json not_before {served.not_before - base:.1f} s; a restart "
        f"of {RESTART_FOR} s at about {SIGTERM_AT} s ran nothing and exited 0"
    )


def synced(served: Served) -> str:
    times = served.archive.syncs
    base = times[0] if times else 0.0
    first = served.of("periodic")
    late = abs(base - first[0]["at"]) if first else None
    if len(times) < 4 or late is None or late > SLACK or not every_period(times[:3], base, 3):
        raise Failure(f"sync calls at {offsets(times, base)} s; first {late} s from the first run")
    failed = [line for line in served.stderr.splitlines() if "ulei-connectors: sync:" in line]
    if failed:
        raise Failure(f"sync failure lines: {failed}")
    return (
        f"sync_every {PERIOD}: C10 sync at {offsets(times[:3], base)} s of the first serve, "
        f"{len(times) - 3} at the restart's start; no sync failure line"
    )


def idle(served: Served) -> str:
    first, last = served.idle
    deltas = {tid: count - first.get(tid, 0) for tid, count in last.items()}
    total = sum(deltas.values())
    if total != 0 or not first:
        raise Failure(f"voluntary_ctxt_switches per thread from {IDLE_FROM} s to {IDLE_TO} s: "
                      f"{deltas}")
    return (
        f"voluntary_ctxt_switches of {len(last)} threads from /proc/<pid>/task/*/status at "
        f"{IDLE_FROM} s and {IDLE_TO} s, with the slow run in flight: "
        f"{sum(first.values())} and {sum(last.values())}, difference 0"
    )


def busy(served: Served) -> str:
    if served.manual is None:
        raise Failure("no manual run")
    done, record = served.manual
    if done.returncode != 1 or record["outcome"] != "busy" or "unit" in record:
        raise Failure(f"manual run exit {done.returncode}, record {record}: {done.stderr.strip()}")
    base = served.of("periodic")[0]["at"] if served.of("periodic") else served.start
    scheduled = [r["at"] for r in served.of("slow", "busy")][1:]
    if not every_period([base, *scheduled], base, 3):
        raise Failure(f"serve's own busy records of slow at {offsets(scheduled, base)} s")
    return (
        f"manual run at {MANUAL_AT} s during the scheduled one: exit 1, outcome busy, no unit; "
        f"{done.stderr.strip()}; serve's own busy records at {offsets(scheduled, base)} s"
    )


def stopped(served: Served) -> str:
    runs = served.of("slow")
    ran = [r for r in runs if r["outcome"] != "busy"]
    if served.exit != 0 or len(ran) != 1 or "unit" not in ran[0]:
        raise Failure(f"exit {served.exit}, slow records {ran}; stderr {served.stderr.strip()}")
    unit = ran[0]["unit"]
    state = subprocess.run(
        ["systemctl", "--user", "is-active", unit], capture_output=True, text=True, check=False,
    ).stdout.strip()
    line = next(
        (line for line in served.stderr.splitlines() if "slow " + ran[0]["outcome"] in line), "",
    )
    if state == "active" or "stopped at the runner's shutdown" not in line:
        raise Failure(f"unit {unit} {state}; line {line!r}")
    return (
        f"exit 0 {served.stopped_after:.1f} s after SIGTERM; record of {unit}: outcome "
        f"{ran[0]['outcome']}, {ran[0]['duration_ms']} ms; unit {state or 'gone'}; {line}"
    )


def schedule(notices: Notices, bindir: Path, work: Path) -> tuple[Bed, bool]:
    bed = Bed(bindir, work)
    for name in ("periodic", "overdue"):
        directory = bed.connector(name, ["bin/ulei-connector-test"])
        shutil.copy2(bed.test, directory / "bin" / "ulei-connector-test")
    limited = json.dumps({
        "type": "error", "code": "rate_limited", "message": "synthetic",
        "retry_after": RETRY_AFTER,
    })
    script(bed, "limited", f"printf '%s\\n' '{limited}'")
    script(bed, "slow", "exec sleep 3600", timeout=3600)
    (bed.state / "overdue").mkdir(parents=True)
    (bed.state / "overdue" / "runner.json").write_text(
        json.dumps({"cursor": None, "last_start": time.time() - PERIOD * 60}), encoding="utf-8",
    )
    bed.write(["periodic", "overdue", "limited", "slow"], {}, sync_every=PERIOD)
    archive = FakeArchive(bed.c10)
    try:
        served = Served(bed, archive)
        served.drive()
        timing, waits, quiet, stop = FOLDS["schedule"]
        passed = notices.check(timing, "connector schedule periods", lambda: periods(served))
        passed &= notices.check(timing, "connector schedule sync", lambda: synced(served))
        passed &= notices.check(
            waits, "connector schedule rate_limited", lambda: rate_limited(served),
        )
        passed &= notices.check(waits, "connector schedule busy", lambda: busy(served))
        passed &= notices.check(quiet, "connector schedule idle", lambda: idle(served))
        passed &= notices.check(stop, "connector schedule SIGTERM", lambda: stopped(served))
    finally:
        archive.close()
    return bed, passed


def run(group: str, bindir: Path, work: Path) -> bool:
    notices = Notices()
    try:
        return run_group(notices, group, bindir, work)
    finally:
        notices.write()


def run_group(notices: Notices, group: str, bindir: Path, work: Path) -> bool:
    first, last = FOLDS[group][0], FOLDS[group][-1]
    abi, line = kernel()
    notices.add(first, f"kernel ({group})", line)
    if abi is None or abi < MIN_ABI:
        annotate(
            "error", f"kernel ({group})",
            f"Landlock ABI {abi if abi is not None else 'absent'}; the Landlock checks need "
            f"ABI {MIN_ABI} or later with landlock in the LSM list",
        )
        return False
    bus = Path(os.environ["XDG_RUNTIME_DIR"]) / "bus"
    if not bus.is_socket():
        raise Failure(f"no user bus at {bus}; systemd-run --wait needs it")
    removed_title = f"connector broker sockets removed ({group})"
    if group == "schedule":
        bed, passed = schedule(notices, bindir, work)
        passed &= notices.check(last, removed_title, lambda: removed(bed))
        return passed
    sources = Sources()
    archive = None
    try:
        bed, paths = setup(bindir, work, sources)
        archive = FakeArchive(bed.c10)
        if group == "sandbox":
            passed = sandbox(notices, bed, archive, paths)
        elif group == "outcomes":
            passed = outcomes(notices, bed, archive)
        else:
            passed = broker(notices, bed, archive, sources)
        passed &= notices.check(last, removed_title, lambda: removed(bed))
        return passed
    finally:
        if archive is not None:
            archive.close()
        sources.close()


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[0] not in GROUPS:
        print(f"usage: connectors_run.py {{{'|'.join(GROUPS)}}} BINDIR WORKDIR", file=sys.stderr)
        return 2
    try:
        passed = run(argv[0], Path(argv[1]).resolve(), Path(argv[2]).resolve())
    except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
        annotate("error", f"connector runs ({argv[0]})", f"setup failed: {e}")
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
