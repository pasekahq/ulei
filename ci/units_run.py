"""Start test of the systemd user units of ``deploy/systemd`` (ADR-0042 section 9).

``python3 ci/units_run.py MODE WORKDIR BINDIR`` needs a running user manager reachable over the
user bus with the seven units and their release binaries installed, and ``ulei-connector-test`` and
``ulei-hive-mcp-fake-hive`` in BINDIR. It clears the units' state directory, writes a synthetic ``ulei.toml`` into the manager's
config directory with every written path in the layout of ADR-0042 section 3, serves a fake qb
gateway under WORKDIR, which must lie outside ``/tmp`` and ``/var/tmp`` that PrivateTmp hides from
the units, and a loopback fake Bot API, and starts ``ulei-memory.service`` alone, which must run
and answer C10 `health` naming its failed pull while no engine runs (ADR-0070), and must take a
40 MiB original through ``ulei-memory import`` in parts within its MemoryMax and give it back by
ranges (ADR-0074), then the units.
It then checks that ``ulei status`` exits 0, that each unit's ReadWritePaths hold exactly the
paths ``ulei.toml`` names for it and that the daemon wrote them, that each process runs with
NoNewPrivs, a seccomp filter and its cgroup limits, and that a stop is clean.
``ulei-connectors.service`` must run the enabled test connector to ``ok`` into the archive of
``ulei-memory.service``, and a stop of it during the run of the enabled ``slow`` connector must
stop that run's unit and leave its record (ADR-0056 section 4).
``ulei-hive-mcp.service`` must answer ``initialize`` and a ``hive_list`` call over its loopback
address, running the fake hive CLI inside the unit, which writes its log in the hive's root
(ADR-0071 section 8), and ``ulei status`` must show it up (ADR-0074). ``ulei-garden.service``
runs without the embedder, its model or ONNX Runtime and must answer C7 `health` with
`dense: off` and a lexical `search` (ADR-0075).

MODE ``default`` runs on the runner's own user namespace setting: a unit the manager starts
without its mount namespace is a notice quoting the journal, and a fatal exit of ``ulei-tg``
(exit 1 on a revoked token) must be restarted. MODE ``sandbox`` needs
``kernel.apparmor_restrict_unprivileged_userns=0``: each unit must run in its own mount namespace
with ``/`` and the home read-only, its ReadWritePaths writable and a private ``/tmp``; then
SIGABRT, what a panic raises under ``panic = "abort"``, must get ``ulei-engine`` restarted with the
owner session's turn in flight and the session idle in ``ulei status``, and ``ulei-tg`` restarted.

The passed checks are folded into four notices, one line per check with its measured value; a
failed check is an error annotation of its own. Every check whose units started runs, and any
failed check makes the exit status 1.
"""

from __future__ import annotations

import base64
import hashlib
import http.client
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Callable

import status_run
from annotations import Notices, annotate
from connectors_run import Bed, run_records, script
from inbox_load import Failure, call, proc_field
from status_run import ANSWERS, read_target
from tg_idle import OWNER_ID, OWNER_SESSION, TOKEN, FakeBotApi

CONNECTORS = "ulei-connectors.service"
MEMORY = "ulei-memory.service"
HIVE_MCP = "ulei-hive-mcp.service"
GARDEN = "ulei-garden.service"
UNITS = (
    "ulei-engine.service", "ulei-inbox.service", "ulei-tg.service", CONNECTORS, MEMORY, HIVE_MCP,
    GARDEN,
)
# The keys whose paths each daemon writes (ADR-0042 section 3).
WRITES = {
    "ulei-inbox.service": ("inbox.root", "inbox.socket"),
    "ulei-engine.service": (
        "engine.journal", "engine.terminal_output", "engine.web_store", "engine.no_ask_guard.log",
        "engine.memory_file", "engine.user_file", "engine.skills_dir", "engine.c6_socket",
    ),
    "ulei-tg.service": ("telegram.attachments_dir",),
    CONNECTORS: ("connectors.state", "connectors.log", "connectors.run_dir"),
    MEMORY: ("memory.archive", "memory.socket"),
    # The fake hive CLI writes `<hive_config>.log` beside the config, in the hive's root.
    HIVE_MCP: ("hive_mcp.hive_config",),
    GARDEN: ("garden.store", "garden.socket", "garden.core_file"),
}
# The bearer token of `ulei-hive-mcp` in the synthetic config.
HIVE_TOKEN = "synthetic-units-hive-token-0123456789abcdef"
MODES = ("default", "sandbox")
START_TIMEOUT = 20.0
STATUS_TIMEOUT = 30.0
RESTART_TIMEOUT = 40.0
POLL_SECONDS = 0.1
POLL_HOLD_SECONDS = 1.0
TURN_TIMEOUT = 20.0
PRIVATE_TMP = (Path("/tmp"), Path("/var/tmp"))
USERNS_SYSCTL = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
# The connectors the runner's unit runs: once each within the test, so no second period falls in.
EVERY = 3600
# The items of one run of the test connector: the runner's C11 `limit`.
ITEMS = 100
RUN_TIMEOUT = 60.0
JOURNAL_TIMEOUT = 10.0


class FakeQb:
    """The C4G requests the fake qb holds unanswered: each `POST /v1/model/turn`."""

    def __init__(self) -> None:
        self.held: list[socket.socket] = []
        self.turn = threading.Event()

    def close(self) -> None:
        for conn in self.held:
            conn.close()


def serve_qb(listener: socket.socket, qb: FakeQb) -> None:
    """Holds each model turn on ``listener`` in ``qb``, answers each other C4G request from
    ``ANSWERS["qb"]``, any other with a C4G `bad_request` error and one notice naming the first
    such request, until closed."""
    noted = False
    while True:
        try:
            conn, _ = listener.accept()
        except OSError:
            return
        try:
            target = read_target(conn)
        except (status_run.Failure, OSError):
            conn.close()
            continue
        if target == "POST /v1/model/turn":
            qb.held.append(conn)
            qb.turn.set()
            continue
        with conn:
            answer = ANSWERS["qb"].get(target)
            status = "200 OK"
            if answer is None:
                status = "404 Not Found"
                answer = {"class": "bad_request", "retryable": False,
                          "message": f"unknown request {target}"}
                if not noted:
                    annotate("notice", "fake qb", f"answered the unknown request {target} with 404")
                    noted = True
            body = json.dumps(answer).encode()
            conn.sendall(
                f"HTTP/1.1 {status}\r\nContent-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n".encode() + body
            )


def host() -> str:
    """The runner settings the units' namespace options depend on."""
    userns = USERNS_SYSCTL.read_text().strip() if USERNS_SYSCTL.exists() else "absent"
    version = subprocess.run(
        ["systemctl", "--version"], capture_output=True, text=True, check=True, timeout=10
    ).stdout.splitlines()[0]
    return f"kernel.apparmor_restrict_unprivileged_userns={userns}, {version}"


def journal_cause(unit: str) -> str:
    """The last line of ``unit``'s journal, the manager's lines on it included, that reads as a
    failure, a namespace failure before any other."""
    done = subprocess.run(
        ["sudo", "-n", "journalctl", "--no-pager", "-o", "cat", "-n", "500",
         f"_SYSTEMD_USER_UNIT={unit}", "+", f"USER_UNIT={unit}"],
        capture_output=True, text=True, check=False, timeout=30,
    )
    words = ("fail", "error", "denied", "not permitted")
    lines = [line for line in done.stdout.splitlines() if any(w in line.lower() for w in words)]
    namespace = [line for line in lines if "namespac" in line.lower()]
    if namespace or lines:
        return (namespace or lines)[-1]
    return f"no failure line in the journal (journalctl exit {done.returncode})"


def layout() -> dict[str, Path]:
    """The directories the units' specifiers `%E`, `%S` and `%t` and `%h/.local/bin` name."""
    home = Path.home()
    runtime = os.environ.get("XDG_RUNTIME_DIR", "")
    if not runtime.startswith("/"):
        raise Failure("XDG_RUNTIME_DIR is not an absolute path: no user manager to reach")

    def xdg(name: str, default: Path) -> Path:
        value = os.environ.get(name, "")
        return Path(value) if value.startswith("/") else default

    return {
        "home": home,
        "config": xdg("XDG_CONFIG_HOME", home / ".config") / "ulei",
        "state": xdg("XDG_STATE_HOME", home / ".local/state") / "ulei",
        "runtime": Path(runtime) / "ulei",
        "connectors_run": Path(runtime) / "ulei-connectors",
        "bin": home / ".local/bin",
    }


def free_port() -> int:
    """A loopback TCP port free right now."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def settings(
    dirs: dict[str, Path], port: int, qb: Path, bed: Bed, bindir: Path,
) -> dict[str, str | int | bool]:
    """The synthetic `ulei.toml` by dotted key."""
    state, engine, run = dirs["state"], dirs["state"] / "engine", dirs["runtime"]
    return {
        "inbox.root": str(state / "inbox"),
        "inbox.socket": str(run / "inbox.sock"),
        "inbox.deliver": True,
        "engine.journal": str(engine / "journal.sqlite3"),
        "engine.c6_socket": str(run / "c6.sock"),
        "engine.owner_session": OWNER_SESSION,
        "engine.default_route": "main",
        "engine.service_unit": "ulei-engine.service",
        "engine.terminal_output": str(engine / "terminal"),
        "engine.web_store": str(engine / "web"),
        "engine.soul_file": str(dirs["config"] / "SOUL.md"),
        "engine.index_file": str(dirs["config"] / "INDEX.md"),
        "engine.memory_file": str(engine / "brain/MEMORY.md"),
        "engine.user_file": str(engine / "brain/USER.md"),
        "engine.skills_dir": str(engine / "skills"),
        "engine.no_ask_guard.log": str(engine / "no-ask.jsonl"),
        "qb.socket": str(qb),
        "telegram.token_file": str(dirs["config"] / "telegram.token"),
        "telegram.owner_ids": OWNER_ID,
        "telegram.attachments_dir": str(state / "telegram/attachments"),
        "telegram.base_url": f"http://127.0.0.1:{port}/bot",
        "memory.archive": str(state / "memory/memory.archive"),
        "memory.socket": str(run / "memory.sock"),
        "connectors.dir": str(bed.dir),
        "connectors.state": str(state / "connectors/state"),
        "connectors.log": str(state / "connectors/runs.jsonl"),
        "connectors.run_dir": str(dirs["connectors_run"]),
        "connectors.enabled": "test,slow",
        "connectors.sync_every": EVERY,
        "hive_mcp.listen": f"127.0.0.1:{free_port()}",
        "hive_mcp.token_file": str(dirs["config"] / "hive-mcp.token"),
        "hive_mcp.hive_command": str(bindir / "ulei-hive-mcp-fake-hive"),
        "hive_mcp.hive_config": str(state / "hive/config.toml"),
        # The inbox creates its producer key at its first start; until then the garden exits 1
        # and is restarted.
        "garden.store": str(state / "garden/garden.sqlite3"),
        "garden.socket": str(run / "garden.sock"),
        "garden.producer_key_file": str(state / "inbox/producer.key"),
        "garden.core_file": str(state / "garden/core.md"),
    }


def write_config(dirs: dict[str, Path], values: dict[str, str | int | bool]) -> Path:
    """Writes `ulei.toml`, the token file, the engine's soul file and the inbox's `ENABLED`;
    returns the config path."""
    dirs["config"].mkdir(parents=True, exist_ok=True)
    Path(str(values["engine.soul_file"])).write_text(
        "You are the synthetic assistant of the units start test.\n", encoding="utf-8"
    )
    token = Path(str(values["telegram.token_file"]))
    token.touch(mode=0o600)
    token.write_text(TOKEN + "\n", encoding="ascii")
    hive_token = Path(str(values["hive_mcp.token_file"]))
    hive_token.touch(mode=0o600)
    hive_token.write_text(HIVE_TOKEN + "\n", encoding="ascii")
    sections: dict[str, list[str]] = {}
    for key, value in values.items():
        section, _, name = key.rpartition(".")
        if isinstance(value, bool):
            text = "true" if value else "false"
        elif isinstance(value, int):
            text = str(value)
        else:
            text = f'"{value}"'
        sections.setdefault(section, []).append(f"{name} = {text}\n")
    path = dirs["config"] / "ulei.toml"
    path.write_text(
        "".join(f"[{section}]\n" + "".join(lines) + "\n" for section, lines in sections.items()),
        encoding="utf-8",
    )
    root = Path(str(values["inbox.root"]))
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    (root / "ENABLED").touch()
    return path


def systemctl(*args: str) -> subprocess.CompletedProcess:
    return subprocess.run(
        ["systemctl", "--user", *args], capture_output=True, text=True, check=False, timeout=60
    )


def show(unit: str, *names: str) -> dict[str, str]:
    done = systemctl("show", unit, *(f"--property={name}" for name in names))
    if done.returncode != 0:
        raise Failure(f"systemctl --user show {unit}: {done.stderr.strip()}")
    return dict(line.partition("=")[::2] for line in done.stdout.splitlines())


def running(unit: str, timeout: float) -> str:
    """The main pid of ``unit`` once it runs."""
    deadline = time.monotonic() + timeout
    while True:
        state = show(unit, "ActiveState", "SubState", "MainPID")
        if state["SubState"] == "running" and state["MainPID"] != "0":
            return state["MainPID"]
        if time.monotonic() > deadline:
            raise Failure(f"{unit} not running within {timeout} s: {state}")
        time.sleep(POLL_SECONDS)


def status_up(ulei: Path, config: Path) -> str:
    """The output of the first `ulei status` that exits 0 within STATUS_TIMEOUT."""
    deadline = time.monotonic() + STATUS_TIMEOUT
    while True:
        done = subprocess.run(
            [str(ulei), "status", "--config", str(config)],
            capture_output=True, text=True, check=False, timeout=15,
        )
        if done.returncode == 0:
            return done.stdout
        if time.monotonic() > deadline:
            raise Failure(
                f"ulei status exited {done.returncode} for {STATUS_TIMEOUT} s:\n"
                f"{done.stdout}{done.stderr}"
            )
        time.sleep(0.5)


def writable(unit: str) -> list[Path]:
    """The unit's ReadWritePaths as the user manager expanded them."""
    value = show(unit, "ReadWritePaths")["ReadWritePaths"]
    return [Path(entry.lstrip("-+")) for entry in value.split()]


def within(path: Path, directory: Path) -> bool:
    return path == directory or directory in path.parents


def check_writes(unit: str, values: dict[str, str | int | bool], dirs: list[Path]) -> None:
    """Every path `ulei.toml` names for ``unit`` lies in one of ``dirs``, and every one of
    ``dirs`` holds such a path."""
    written = [Path(str(values[key])) for key in WRITES[unit]]
    for key, path in zip(WRITES[unit], written):
        if not any(within(path, d) for d in dirs):
            raise Failure(f"{unit}: {key} {path} is outside ReadWritePaths {dirs}")
    for d in dirs:
        if not any(within(path, d) for path in written):
            raise Failure(f"{unit}: ReadWritePaths entry {d} holds no path ulei.toml names for it")


def mounts(pid: str) -> list[tuple[Path, str, bool]]:
    """Mount point, mount root and read-only flag of each mount of ``pid``, in mountinfo order."""
    table = []
    for line in Path(f"/proc/{pid}/mountinfo").read_text(encoding="utf-8").splitlines():
        fields = line.split()
        point = fields[4].encode().decode("unicode_escape")
        table.append((Path(point), fields[3], "ro" in fields[5].split(",")))
    return table


def read_only(table: list[tuple[Path, str, bool]], path: Path) -> bool:
    """Whether the topmost mount holding ``path`` is read-only."""
    best: tuple[Path, str, bool] | None = None
    for entry in table:
        if within(path, entry[0]) and (best is None or len(entry[0].parts) >= len(best[0].parts)):
            best = entry
    if best is None:
        raise Failure(f"no mount holds {path}")
    return best[2]


def check_process(unit: str, pid: str) -> str:
    """NoNewPrivs, the seccomp filter and the cgroup limits of ``unit``'s process ``pid``."""
    nnp = proc_field(int(pid), "status", "NoNewPrivs")
    seccomp = proc_field(int(pid), "status", "Seccomp")
    if (nnp, seccomp) != (1, 2):
        raise Failure(f"NoNewPrivs {nnp}, Seccomp {seccomp}; expected 1 and 2 (filter)")
    cgroup = next(
        line.removeprefix("0::")
        for line in Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines()
        if line.startswith("0::")
    )
    limits = show(unit, "MemoryMax", "TasksMax")
    for name, prop in (("memory.max", "MemoryMax"), ("pids.max", "TasksMax")):
        file = Path("/sys/fs/cgroup" + cgroup) / name
        if not file.exists():
            raise Failure(f"{unit}: {file} is missing, the user manager has no such controller")
        if file.read_text(encoding="ascii").strip() != limits[prop]:
            raise Failure(f"{unit}: {file} is {file.read_text().strip()}, {prop} is {limits[prop]}")
    return (
        f"NoNewPrivs 1, seccomp filter, memory.max {limits['MemoryMax']}, "
        f"pids.max {limits['TasksMax']}"
    )


def check_mounts(unit: str, pid: str, home: Path, dirs: list[Path]) -> str:
    """The mount namespace of ``unit``'s process ``pid``: `/` and the home read-only, ``dirs``
    writable, a private `/tmp`."""
    table = mounts(pid)
    for path, want in ((Path("/"), True), (home, True), *((d, False) for d in dirs)):
        if read_only(table, path) != want:
            raise Failure(f"{unit}: {path} is {'writable' if want else 'read-only'} in pid {pid}")
    tmp = [entry for entry in table if entry[0] == Path("/tmp")]
    if not tmp or tmp[-1][1] == "/tmp":
        raise Failure(f"{unit}: /tmp of pid {pid} is the host's, PrivateTmp is not applied")
    return (
        f"own mount namespace, / and home read-only, {' '.join(map(str, dirs))} writable, "
        "private /tmp"
    )


def written(unit: str, values: dict[str, str | int | bool]) -> list[Path]:
    """The paths the daemon of ``unit`` creates at start."""
    if unit == "ulei-inbox.service":
        root = Path(str(values["inbox.root"]))
        return [root / "events.sqlite3", Path(str(values["inbox.socket"]))]
    if unit == "ulei-engine.service":
        return [Path(str(values["engine.journal"])), Path(str(values["engine.c6_socket"]))]
    if unit == MEMORY:
        return [Path(str(values["memory.archive"])), Path(str(values["memory.socket"]))]
    if unit == CONNECTORS:
        state = Path(str(values["connectors.state"]))
        return [state / "test/runner.json", Path(str(values["connectors.log"]))]
    if unit == HIVE_MCP:
        return [Path(str(values["hive_mcp.hive_config"]) + ".log")]
    if unit == GARDEN:
        return [Path(str(values[key])) for key in WRITES[GARDEN]]
    return [Path(str(values["telegram.attachments_dir"]))]


def restarted(unit: str, before: dict[str, str]) -> tuple[dict[str, str], dict[str, str]]:
    """Waits for systemd to restart ``unit`` once after its ``before`` MainPID and NRestarts; the
    state the process ended in and the state of the restarted one."""
    deadline = time.monotonic() + RESTART_TIMEOUT
    ended: dict[str, str] | None = None
    while True:
        state = show(unit, "SubState", "MainPID", "NRestarts", "ExecMainCode", "ExecMainStatus")
        if state["SubState"] == "auto-restart":
            ended = state
        if (
            ended is not None
            and state["SubState"] == "running"
            and state["MainPID"] not in ("0", before["MainPID"])
            and int(state["NRestarts"]) == int(before["NRestarts"]) + 1
        ):
            break
        if time.monotonic() > deadline:
            raise Failure(f"{unit} was not restarted within {RESTART_TIMEOUT} s: {state}")
        time.sleep(POLL_SECONDS)
    return ended, state


def fatal_restart(api: FakeBotApi) -> str:
    """Revokes the token once, so `ulei-tg` exits 1, and waits for systemd to restart it."""
    unit = "ulei-tg.service"
    before = show(unit, "MainPID", "NRestarts")
    api.revoke.set()
    ended, state = restarted(unit, before)
    # ExecMainCode 1 is CLD_EXITED: the process exited rather than died of a signal.
    if (ended["ExecMainCode"], ended["ExecMainStatus"]) != ("1", "1"):
        raise Failure(
            f"{unit} ended with code {ended['ExecMainCode']} status {ended['ExecMainStatus']}, "
            "expected exit status 1"
        )
    return f"pid {before['MainPID']} exited 1, restarted as pid {state['MainPID']}"


def aborted(unit: str) -> str:
    """Sends SIGABRT to ``unit``'s process and waits for systemd to restart it."""
    before = show(unit, "MainPID", "NRestarts")
    os.kill(int(before["MainPID"]), signal.SIGABRT)
    ended, state = restarted(unit, before)
    # ExecMainCode 2 is CLD_KILLED and 3 CLD_DUMPED: the process died of the signal.
    if ended["ExecMainCode"] not in ("2", "3") or ended["ExecMainStatus"] != str(int(signal.SIGABRT)):
        raise Failure(
            f"{unit} ended with code {ended['ExecMainCode']} status {ended['ExecMainStatus']}, "
            f"expected signal {int(signal.SIGABRT)}"
        )
    return f"pid {before['MainPID']} died of SIGABRT, restarted as pid {state['MainPID']}"


def engine_abort(
    qb: FakeQb, values: dict[str, str | int | bool], ulei: Path, config: Path
) -> str:
    """Starts a turn of the owner session that the fake qb holds, aborts `ulei-engine` and checks
    that `ulei status` exits 0 with the session idle after the restart."""
    socket_path = str(values["engine.c6_socket"])
    session = str(values["engine.owner_session"])
    submitted, _ = call(socket_path, "submit_turn", {"session": session, "prompt": "abort probe"})
    if not submitted["accepted"]:
        raise Failure(f"submit_turn refused: {submitted}")
    turn = submitted["turn"]
    if not qb.turn.wait(TURN_TIMEOUT):
        report, _ = call(socket_path, "status", {"session": session, "turn": turn})
        raise Failure(
            f"turn {turn} did not reach the fake qb in {TURN_TIMEOUT} s: {report['turn']}"
        )
    before, _ = call(socket_path, "status", {"session": session})
    if not before["running"]:
        raise Failure(f"the session does not run turn {turn}: {before}")
    restart_line = aborted("ulei-engine.service")
    lines = status_up(ulei, config).splitlines()
    session_line = next((line for line in lines if line.startswith("session: ")), "")
    if not session_line.startswith(f"session: {session} idle "):
        raise Failure(f"{restart_line}; ulei status shows {session_line!r}")
    last = next((line for line in lines if line.startswith("last turn: ")), "")
    return (
        f"turn {turn} reached the fake qb and runs; {restart_line}; ulei status exit 0, "
        f"{session_line}; {last}"
    )


def tg_abort(ulei: Path, config: Path) -> str:
    restart_line = aborted("ulei-tg.service")
    status_up(ulei, config)
    return f"{restart_line}; ulei status exit 0 again"


def stop_clean(api: FakeBotApi) -> str:
    """Stops the units; each must end inactive with result success, and the fake Bot API must
    have seen no unexpected request."""
    done = systemctl("stop", *UNITS)
    if done.returncode != 0:
        raise Failure(f"systemctl --user stop: {done.stderr.strip()}")
    for unit in UNITS:
        state = show(unit, "ActiveState", "Result")
        if state != {"ActiveState": "inactive", "Result": "success"}:
            raise Failure(f"{unit} after stop: {state}")
    if api.errors:
        raise Failure("fake Bot API: " + "; ".join(api.errors))
    return "every unit inactive with result success"


def records(values: dict[str, str | int | bool]) -> list[dict]:
    return run_records(Path(str(values["connectors.log"])))


def poll(what: str, timeout: float, probe: Callable[[], list]) -> list:
    """The first non-empty answer of ``probe`` within ``timeout`` seconds."""
    deadline = time.monotonic() + timeout
    while True:
        found = probe()
        if found:
            return found
        if time.monotonic() > deadline:
            raise Failure(f"no {what} within {timeout} s")
        time.sleep(POLL_SECONDS)


def journal(since: int, needle: str) -> list[str]:
    """The lines of ``CONNECTORS`` in the journal from the Unix time ``since`` that hold
    ``needle``."""
    done = subprocess.run(
        ["sudo", "-n", "journalctl", "--no-pager", "-o", "cat", f"--since=@{since}",
         f"_SYSTEMD_USER_UNIT={CONNECTORS}"],
        capture_output=True, text=True, check=False, timeout=30,
    )
    return [line for line in done.stdout.splitlines() if needle in line]


def health(values: dict[str, str | int | bool]) -> dict:
    """The archive's C10 `health` on `memory.socket`."""
    return call(str(values["memory.socket"]), "health", {})[0]


def memory_alone(values: dict[str, str | int | bool], settings_line: str) -> str:
    """`ulei-memory.service` started while no engine runs: it runs, binds `memory.socket` and
    answers `health` with an empty archive and its failed pull."""
    line = start(MEMORY, settings_line)
    engine = show("ulei-engine.service", "ActiveState")["ActiveState"]
    if engine == "active":
        raise Failure(f"ulei-engine.service is active before the start of {MEMORY}")

    def answered() -> list[dict]:
        try:
            return [health(values)]
        except (Failure, OSError):
            return []

    try:
        answer = poll("C10 health on memory.socket", START_TIMEOUT, answered)[0]
    except Failure as e:
        raise Failure(f"{e}; journal: {journal_cause(MEMORY)}") from None
    if answer["items"] != 0 or not answer.get("sync_error"):
        raise Failure(f"health without the engine: {answer}")
    return (
        f"ulei-engine.service {engine}; {MEMORY} {line}; health: version {answer['version']}, "
        f"items 0, sync_error {answer['sync_error']!r}"
    )


BIG_ITEM = 40 * 1024 * 1024
RANGE = 512 * 1024
IMPORT_TIMEOUT = 600


def memory_big(values: dict[str, str | int | bool], ulei_memory: Path, config: Path,
               work: Path) -> str:
    """A 40 MiB original through `ulei-memory import` with `bytes_path` goes in parts into the
    archive of the unit, which stays within its MemoryMax without a restart; `read` by ranges
    gives back its SHA-256, and the owner's delete removes it again."""
    original = os.urandom(BIG_ITEM)
    item_id = hashlib.sha256(original).hexdigest()
    path = work / "big-item.bin"
    path.write_bytes(original)
    del original
    pid = show(MEMORY, "MainPID")["MainPID"]
    line = json.dumps({
        "source": "library", "external_id": "ci/big-item.bin", "kind": "application/octet-stream",
        "text": "synthetic big item", "bytes_path": str(path), "id": item_id,
    })
    started = time.monotonic()
    done = subprocess.run(
        [str(ulei_memory), "import", "--config", str(config)], input=line + "\n",
        capture_output=True, text=True, check=False, timeout=IMPORT_TIMEOUT,
    )
    took = time.monotonic() - started
    path.unlink()
    if done.returncode != 0 or done.stderr != "import: 1 lines, 1 stored, 0 duplicate, 0 refused\n":
        raise Failure(f"import exit {done.returncode}: {done.stderr.strip()}")
    digest, offset, size = hashlib.sha256(), 0, None
    while size is None or offset < size:
        answer = call(str(values["memory.socket"]), "read",
                      {"id": item_id, "offset": offset, "length": RANGE})[0]
        try:
            chunk = base64.b64decode(answer["bytes_b64"], validate=True)
        except ValueError as e:
            raise Failure(f"read at {offset}: {e}") from None
        size = answer["len"]
        if not chunk:
            break
        digest.update(chunk)
        offset += len(chunk)
    if (offset, digest.hexdigest()) != (BIG_ITEM, item_id):
        raise Failure(f"read back {offset} bytes with SHA-256 {digest.hexdigest()}")
    hwm = proc_field(int(pid), "status", "VmHWM")
    cgroup = next(
        line.removeprefix("0::")
        for line in Path(f"/proc/{pid}/cgroup").read_text(encoding="utf-8").splitlines()
        if line.startswith("0::")
    )
    group = Path("/sys/fs/cgroup" + cgroup)
    peak = (group / "memory.peak").read_text(encoding="ascii").strip()
    events = dict(
        line.split() for line in (group / "memory.events").read_text(encoding="ascii").splitlines()
    )
    state = show(MEMORY, "MainPID", "NRestarts", "MemoryMax")
    if state["MainPID"] != pid or state["NRestarts"] != "0" or events.get("oom_kill") != "0":
        raise Failure(f"{MEMORY} after the import: {state}, memory.events {events}")
    if hwm * 1024 * 2 > int(state["MemoryMax"]):
        raise Failure(f"VmHWM {hwm} kB is over half of MemoryMax {state['MemoryMax']} B")
    removed = subprocess.run(
        [str(ulei_memory), "delete", item_id, "--reason", "ci", "--config", str(config)],
        capture_output=True, text=True, check=False, timeout=60,
    )
    if removed.returncode != 0 or health(values)["items"] != 0:
        raise Failure(f"delete exit {removed.returncode}: {removed.stderr.strip()}")
    return (
        f"40 MiB in {took:.1f} s through ulei-memory import in parts, read back by ranges with "
        f"its SHA-256; pid {pid} VmHWM {hwm} kB, memory.peak {peak} B of MemoryMax "
        f"{state['MemoryMax']} B, oom_kill 0, no restart; deleted again"
    )


def connector_ok(values: dict[str, str | int | bool]) -> str:
    """The unit's `serve` runs the enabled test connector, never run before, to `ok`, and the
    archive of `ulei-memory.service` holds its items."""
    try:
        ran = poll("run record of test", RUN_TIMEOUT, lambda: [
            r for r in records(values) if r["connector"] == "test"
        ])
    except Failure as e:
        raise Failure(f"{e}; journal: {journal_cause(CONNECTORS)}") from None
    record = ran[0]
    answer = health(values)
    if record["outcome"] != "ok" or record["stored"] != ITEMS or answer["items"] != ITEMS:
        raise Failure(f"run record {record}; archive health {answer}")
    pid = show(CONNECTORS, "MainPID")["MainPID"]
    return (
        f"pid {pid} ran test in the unit {record['unit']}: outcome ok, stored {record['stored']} "
        f"in {MEMORY}, whose health counts {answer['items']} items, "
        f"landlock_abi {record['landlock_abi']}, "
        f"{record['duration_ms']} ms; systemd-run reached the manager, and the manager the broker "
        f"socket in {values['connectors.run_dir']} through OpenFile="
    )


def connector_stop(values: dict[str, str | int | bool]) -> str:
    """`systemctl --user stop` of the runner's unit while the slow connector runs stops that
    run's unit, and `serve` writes its record and exits 0."""
    found = poll("running unit of slow", RUN_TIMEOUT, lambda: systemctl(
        "list-units", "--plain", "--no-legend", "--state=active", "ulei-connector-slow-*.service",
    ).stdout.split()[:1])
    unit = found[0]
    pid = show(CONNECTORS, "MainPID")["MainPID"]
    since = int(time.time())
    done = systemctl("stop", CONNECTORS)
    if done.returncode != 0:
        raise Failure(f"systemctl --user stop {CONNECTORS}: {done.stderr.strip()}")
    state = show(CONNECTORS, "ActiveState", "Result")
    if state != {"ActiveState": "inactive", "Result": "success"}:
        raise Failure(f"{CONNECTORS} after stop: {state}")
    left = systemctl("is-active", unit).stdout.strip()
    if left == "active":
        raise Failure(f"{unit} is still active after the stop of {CONNECTORS}")
    mine = [r for r in records(values) if r.get("unit") == unit]
    if not mine:
        raise Failure(f"no run record of {unit} in {values['connectors.log']}")
    line = poll(
        "journal line of the stopped run", JOURNAL_TIMEOUT,
        lambda: journal(since, "stopped at the runner's shutdown"),
    )[-1]
    # serve writes this line only on the way to exit 0 after its handler stopped the runs.
    stopped = poll(
        "journal line of serve's exit", JOURNAL_TIMEOUT,
        lambda: journal(since, "ulei-connectors: stopped"),
    )[-1]
    return (
        f"{unit} ran; {CONNECTORS} pid {pid} after the stop: inactive with result success, "
        f"journal {stopped!r}; {unit} {left or 'gone'}; its record: outcome "
        f"{mine[0]['outcome']}, {mine[0]['duration_ms']} ms; journal: {line}"
    )


def start(unit: str, settings_line: str) -> str:
    try:
        pid = running(unit, START_TIMEOUT)
    except Failure as e:
        raise Failure(f"{e}; journal: {journal_cause(unit)}; {settings_line}") from None
    return f"active, pid {pid}; {settings_line}"


def status(ulei: Path, config: Path) -> str:
    return "exit 0\n" + status_up(ulei, config)


def sandbox(unit: str, values: dict[str, str | int | bool], home: Path, mode: str) -> str:
    dirs = writable(unit)
    check_writes(unit, values, dirs)
    missing = [str(path) for path in written(unit, values) if not path.exists()]
    if missing:
        raise Failure(f"the daemon did not write {' '.join(missing)}")
    pid = show(unit, "MainPID")["MainPID"]
    process = check_process(unit, pid)
    wrote = f"wrote {' '.join(str(path) for path in written(unit, values))}"
    if os.readlink(f"/proc/{pid}/ns/mnt") != os.readlink("/proc/self/ns/mnt"):
        return f"pid {pid}: {check_mounts(unit, pid, dirs=dirs, home=home)}; {wrote}; {process}"
    cause = f"journal: {journal_cause(unit)}; {host()}"
    if mode == "sandbox":
        raise Failure(
            f"pid {pid} runs in the host mount namespace, so ProtectSystem=strict, "
            f"ProtectHome=read-only, ReadWritePaths and PrivateTmp are not applied; {cause}"
        )
    return (
        f"pid {pid}: host mount namespace, the four namespace options not applied; {cause}; "
        f"{wrote}; {process}"
    )


def mcp_post(port: int, message: dict, session: str | None) -> tuple[int, dict[str, str], dict]:
    """One MCP POST to `ulei-hive-mcp` on ``port`` with the bearer token."""
    headers = {
        "Host": f"127.0.0.1:{port}", "Authorization": f"Bearer {HIVE_TOKEN}",
        "Content-Type": "application/json", "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    try:
        conn.request("POST", "/mcp", body=json.dumps(message), headers=headers)
        answer = conn.getresponse()
        body = answer.read()
        return answer.status, {k.lower(): v for k, v in answer.getheaders()}, (
            json.loads(body) if body else {}
        )
    finally:
        conn.close()


def hive_call(values: dict[str, str | int | bool], ulei: Path, config: Path) -> str:
    """`initialize` and a `hive_list` call through the unit's server: the fake hive CLI runs
    inside the unit and writes its log in the hive's root; then `ulei status` shows it up."""
    port = int(str(values["hive_mcp.listen"]).rpartition(":")[2])
    deadline = time.monotonic() + START_TIMEOUT
    while True:
        try:
            status, headers, _ = mcp_post(port, {
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-11-25", "capabilities": {}},
            }, None)
            break
        except OSError:
            if time.monotonic() > deadline:
                raise Failure(f"ulei-hive-mcp did not accept on port {port}") from None
            time.sleep(POLL_SECONDS)
    session = headers.get("mcp-session-id")
    if status != 200 or not session:
        raise Failure(f"initialize answered {status} without a session")
    status, _, answer = mcp_post(port, {
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "hive_list", "arguments": {}},
    }, session)
    result = answer.get("result", {})
    if status != 200 or result.get("isError") is not False:
        raise Failure(f"hive_list answered {status}: {answer}")
    cards = len(result["structuredContent"]["cards"])
    want = f"hive-mcp: up {values['hive_mcp.listen']}"
    lines = status_up(ulei, config).splitlines()
    if want not in lines:
        raise Failure(f"ulei status has no line {want!r}: {lines}")
    return (f"initialize and hive_list on port {port}: {cards} cards from the fake hive CLI; "
            f"ulei status: {want!r}")


def garden_lexical(values: dict[str, str | int | bool]) -> str:
    """C7 `health` and `search` of `ulei-garden.service` without the embedder: the dense side
    off, so the search is lexical and, the dense side not being down, carries no `lexical_only`."""
    socket_path = str(values["garden.socket"])

    def answered() -> list[dict]:
        try:
            return [call(socket_path, "health", {})[0]]
        except (Failure, OSError):
            return []

    try:
        answer = poll("C7 health on garden.socket", START_TIMEOUT, answered)[0]
    except Failure as e:
        raise Failure(f"{e}; journal: {journal_cause(GARDEN)}") from None
    if answer.get("dense") != "off":
        raise Failure(f"health without the embedder: {answer}")
    found = call(socket_path, "search", {"q": "synthetic garden query", "no_miss": True})[0]
    if "facts" not in found or "lexical_only" in found:
        raise Failure(f"search without the embedder: {found}")
    return (
        f"health: version {answer['version']}, dense off; search answered lexically with "
        f"{len(found['facts'])} facts"
    )


def restart(api: FakeBotApi, ulei: Path, config: Path) -> str:
    restarted = fatal_restart(api)
    status_up(ulei, config)
    return f"{restarted}; ulei status exit 0 again"


def run(mode: str, work: Path, bindir: Path) -> bool:
    """Runs every check of ``mode`` whose units started; whether all passed."""
    notices = Notices()
    try:
        return run_checks(notices, mode, work, bindir)
    finally:
        notices.write()


def run_checks(notices: Notices, mode: str, work: Path, bindir: Path) -> bool:
    dirs = layout()
    if mode == "sandbox" and USERNS_SYSCTL.read_text().strip() != "0":
        raise Failure(f"{USERNS_SYSCTL} is {USERNS_SYSCTL.read_text().strip()}, expected 0")
    hidden = [d for d in PRIVATE_TMP if work.resolve().is_relative_to(d)]
    if hidden:
        raise Failure(f"WORKDIR {work} is under {hidden[0]}, which PrivateTmp hides from the units")
    if dirs["state"].exists():
        shutil.rmtree(dirs["state"])
    bed = Bed(bindir, work)
    test = bed.connector("test", ["bin/ulei-connector-test"], every=EVERY)
    shutil.copy2(bed.test, test / "bin" / "ulei-connector-test")
    script(bed, "slow", "exec sleep 3600", timeout=3600, every=EVERY)
    qb = work / "qb.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(qb))
    listener.listen()
    fake_qb = FakeQb()
    threading.Thread(target=serve_qb, args=(listener, fake_qb), daemon=True).start()
    api = FakeBotApi(POLL_HOLD_SECONDS)
    threading.Thread(target=api.serve_forever, daemon=True).start()
    starts, sandboxes, ends = (
        f"{mode} units: start and ulei status", f"{mode} units: sandbox",
        f"{mode} units: restarts and stop",
    )
    runs = f"{mode} {CONNECTORS}: a run and a stop during a run"
    try:
        values = settings(dirs, api.server_port, qb, bed, bindir)
        config = write_config(dirs, values)
        ulei = dirs["bin"] / "ulei"
        settings_line = host()
        done = systemctl("start", MEMORY)
        print(f"systemctl --user start {MEMORY}: exit {done.returncode} {done.stderr.strip()}")
        alone = notices.check(
            starts, f"{mode} start {MEMORY} without the engine",
            lambda: memory_alone(values, settings_line),
        )
        if alone:
            alone = notices.check(
                starts, f"{mode} {MEMORY} takes a 40 MiB item in parts",
                lambda: memory_big(values, dirs["bin"] / "ulei-memory", config, work),
            )
        done = systemctl("start", *UNITS)
        print(f"systemctl --user start: exit {done.returncode} {done.stderr.strip()}", flush=True)
        started = [u for u in UNITS if notices.check(
            starts, f"{mode} start {u}", lambda u=u: start(u, settings_line)
        )]
        passed = alone and len(started) == len(UNITS)
        passed &= notices.check(starts, f"{mode} ulei status", lambda: status(ulei, config))
        if HIVE_MCP in started:
            passed &= notices.check(
                starts, f"{mode} {HIVE_MCP} call", lambda: hive_call(values, ulei, config),
            )
        if GARDEN in started:
            passed &= notices.check(
                starts, f"{mode} {GARDEN} without the model", lambda: garden_lexical(values),
            )
        if CONNECTORS in started:
            passed &= notices.check(
                runs, f"{mode} {CONNECTORS} runs test", lambda: connector_ok(values),
            )
        for unit in started:
            passed &= notices.check(
                sandboxes, f"{mode} sandbox {unit}",
                lambda u=unit: sandbox(u, values, dirs["home"], mode),
            )
        if mode == "default" and "ulei-tg.service" in started:
            passed &= notices.check(
                ends, "fatal exit of ulei-tg.service", lambda: restart(api, ulei, config),
            )
        if mode == "sandbox" and len(started) == len(UNITS):
            passed &= notices.check(
                ends, "SIGABRT of ulei-engine.service",
                lambda: engine_abort(fake_qb, values, ulei, config),
            )
            passed &= notices.check(
                ends, "SIGABRT of ulei-tg.service", lambda: tg_abort(ulei, config),
            )
        if CONNECTORS in started:
            passed &= notices.check(
                runs, f"{mode} stop of {CONNECTORS} during a run", lambda: connector_stop(values),
            )
        passed &= notices.check(ends, f"{mode} stop", lambda: stop_clean(api))
        return passed
    finally:
        systemctl("stop", *UNITS)
        systemctl("stop", "ulei-connector-*.service")
        api.release.set()
        api.shutdown()
        api.server_close()
        listener.close()
        fake_qb.close()


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[0] not in MODES:
        print("usage: units_run.py default|sandbox WORKDIR BINDIR", file=sys.stderr)
        return 2
    try:
        passed = run(argv[0], Path(argv[1]), Path(argv[2]).resolve())
    except (Failure, OSError, subprocess.SubprocessError) as e:
        annotate("error", f"{argv[0]} start test", f"setup failed: {e}")
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
