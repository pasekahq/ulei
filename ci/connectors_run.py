"""Runs of the source connectors in their transient units (ADR-0056 sections 5.1, 5.2 and 5.7).

``python3 ci/connectors_run.py BINDIR WORKDIR`` needs a running user manager reachable over the
user bus, which ``systemd-run --wait`` uses, and ``ulei-connectors`` and ``ulei-connector-test``
in BINDIR. It prints the kernel, the LSM list and the Landlock ABI as a notice and fails without
Landlock ABI 3 or later. It serves a fake C10 socket under WORKDIR that answers each `put` with
`stored` or `duplicate`, writes a synthetic ``ulei.toml`` with its connectors, keys, grants and
fake archive file, and checks: a run lands its items in the fake C10 and names its Landlock ABI;
the test connector's probes see `socket(AF_INET)` and `socket(AF_UNIX)` refused with
`EAFNOSUPPORT` or `EPERM`; its file probes see `EACCES` outside its paths and grants with the
fake archive's bytes unchanged, and succeed inside them; with `landlock_create_ruleset` answering
an injected `ENOSYS` or `EOPNOTSUPP` a run is refused with its line, and a connector under
`landlock = "off"` runs and says so; a `systemd-run` that cannot start the unit is `refused`; a
connector sleeping past its timeout ends `timeout` with its stored cursor kept; and
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
# Below it truncation is never checked (ADR-0056 section 5.2).
MIN_ABI = 3
REFUSED = ("EAFNOSUPPORT errno 97", "EPERM errno 1")
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
        self.c10 = work / "c10.sock"
        self.config = work / "ulei.toml"

    def connector(
        self, name: str, command: list[str], timeout: int | None = None, key: str | None = None,
    ) -> Path:
        directory = self.dir / name
        (directory / "bin").mkdir(parents=True)
        manifest = {
            "name": name, "version": "0.1.0", "contract": "0.1.0", "transport": "stdio",
            "command": command, "schedule": {"every": 60}, "source": {"addresses": []},
        }
        if timeout is not None:
            manifest["timeout"] = timeout
        if key is not None:
            manifest["key"] = key
        (directory / "connector.json").write_text(json.dumps(manifest), encoding="utf-8")
        return directory

    def write(self, enabled: list[str], owner: dict[str, dict[str, str]]) -> None:
        """``ulei.toml`` with ``enabled`` and the ``[connectors.<name>]`` records of ``owner``."""
        text = (
            f'[memory]\nsocket = "{self.c10}"\n\n[connectors]\ndir = "{self.dir}"\n'
            f'state = "{self.state}"\nlog = "{self.log}"\nenabled = "{",".join(enabled)}"\n'
        )
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


def run(bindir: Path, work: Path) -> bool:
    abi, line = kernel()
    annotate("notice", "kernel", line)
    if abi is None or abi < MIN_ABI:
        annotate(
            "error", "kernel",
            f"Landlock ABI {abi if abi is not None else 'absent'}; the Landlock checks need "
            f"ABI {MIN_ABI} or later with landlock in the LSM list",
        )
        return False
    bus = Path(os.environ["XDG_RUNTIME_DIR"]) / "bus"
    if not bus.is_socket():
        raise Failure(f"no user bus at {bus}; systemd-run --wait needs it")
    bed = Bed(bindir, work)
    paths = Files(bed, "files")
    paths.make()
    for name, command, key in (
        ("test", ["bin/ulei-connector-test"], None),
        ("unconfined", ["bin/ulei-connector-test"], None),
        ("probe", ["bin/ulei-connector-test", "--probe-inet", "--probe-unix", str(bed.c10)],
         "probe"),
        (paths.name, paths.command(bed.config), "files"),
    ):
        directory = bed.connector(name, command, key=key)
        shutil.copy2(bed.test, directory / "bin" / "ulei-connector-test")
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
        ["test", "unconfined", "probe", paths.name, "sleeper"],
        {
            "unconfined": {"landlock": "off"},
            "probe": {"key_file": str(paths.other_key)},
            paths.name: paths.owner(),
        },
    )
    archive = FakeArchive(bed.c10)
    try:
        passed = check("connector run lands items in C10", lambda: lands(bed, archive))
        passed &= check("connector sockets refused", lambda: probes(bed, archive))
        try:
            refusals, allowed = files(bed, archive, paths)
        except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
            annotate("error", "connector Landlock refusals", str(e))
            passed = False
        else:
            annotate("notice", "connector Landlock refusals", refusals)
            annotate("notice", "connector Landlock paths and grants", allowed)
        for errno_name in ("ENOSYS", "EOPNOTSUPP"):
            passed &= check(
                f"connector refused on injected {errno_name}",
                lambda errno_name=errno_name: injected(bed, errno_name),
            )
        passed &= check("connector landlock off", lambda: opted_out(bed))
        passed &= check("connector unit fails to start", lambda: unit_start(bed))
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
