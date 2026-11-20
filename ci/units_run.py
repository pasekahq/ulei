"""Start test of the systemd user units of ``deploy/systemd`` (ADR-0042 section 9).

``python3 ci/units_run.py MODE WORKDIR`` needs a running user manager with the three units and the
release binaries installed by ``deploy/install.sh``. It clears the units' state directory, writes a
synthetic ``ulei.toml`` into the manager's config directory with every written path in the layout
of ADR-0042 section 3, serves a fake qb gateway under WORKDIR and a loopback fake Bot API, and
starts the units. It then checks that ``ulei status`` exits 0, that each unit's ReadWritePaths hold
exactly the paths ``ulei.toml`` names for it and that the daemon wrote them, that each process runs
with NoNewPrivs, a seccomp filter and its cgroup limits, and that a stop is clean.

MODE ``default`` runs on the runner's own user namespace setting: a unit the manager starts
without its mount namespace is a notice quoting the journal, and a fatal exit of ``ulei-tg``
(exit 1 on a revoked token) must be restarted. MODE ``sandbox`` needs
``kernel.apparmor_restrict_unprivileged_userns=0``: each unit must run in its own mount namespace
with ``/`` and the home read-only, its ReadWritePaths writable and a private ``/tmp``; then
SIGABRT, what a panic raises under ``panic = "abort"``, must get ``ulei-engine`` restarted with the
owner session's turn in flight and the session idle in ``ulei status``, and ``ulei-tg`` restarted.

Each check's result is one GitHub annotation, at most ten in all; every check whose units started
runs, and any failed check makes the exit status 1.
"""

from __future__ import annotations

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
from inbox_load import Failure, call, proc_field
from status_run import ANSWERS, read_target
from tg_idle import OWNER_ID, OWNER_SESSION, TOKEN, FakeBotApi

UNITS = ("ulei-engine.service", "ulei-inbox.service", "ulei-tg.service")
# The keys whose paths each daemon writes (ADR-0042 section 3).
WRITES = {
    "ulei-inbox.service": ("inbox.root", "inbox.socket"),
    "ulei-engine.service": (
        "engine.journal", "engine.terminal_output", "engine.web_store", "engine.no_ask_guard.log",
        "engine.memory_file", "engine.user_file", "engine.skills_dir", "engine.c6_socket",
    ),
    "ulei-tg.service": ("telegram.attachments_dir",),
}
MODES = ("default", "sandbox")
START_TIMEOUT = 20.0
STATUS_TIMEOUT = 30.0
RESTART_TIMEOUT = 40.0
POLL_SECONDS = 0.1
POLL_HOLD_SECONDS = 1.0
TURN_TIMEOUT = 20.0
USERNS_SYSCTL = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")


def annotate(level: str, title: str, message: str) -> None:
    """One GitHub workflow annotation, which the public check-runs API serves without the log."""
    text = message.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")
    print(f"::{level} title={title}::{text}", flush=True)


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
        "bin": home / ".local/bin",
    }


def settings(dirs: dict[str, Path], port: int, qb: Path) -> dict[str, str | int | bool]:
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
        "engine.memory_file": str(engine / "brain/MEMORY.md"),
        "engine.user_file": str(engine / "brain/USER.md"),
        "engine.skills_dir": str(engine / "skills"),
        "engine.no_ask_guard.log": str(engine / "no-ask.jsonl"),
        "qb.socket": str(qb),
        "telegram.token_file": str(dirs["config"] / "telegram.token"),
        "telegram.owner_ids": OWNER_ID,
        "telegram.attachments_dir": str(state / "telegram/attachments"),
        "telegram.base_url": f"http://127.0.0.1:{port}/bot",
    }


def write_config(dirs: dict[str, Path], values: dict[str, str | int | bool]) -> Path:
    """Writes `ulei.toml`, the token file and the inbox's `ENABLED`; returns the config path."""
    dirs["config"].mkdir(parents=True, exist_ok=True)
    token = Path(str(values["telegram.token_file"]))
    token.touch(mode=0o600)
    token.write_text(TOKEN + "\n", encoding="ascii")
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
        raise Failure(f"turn {turn} did not reach the fake qb in {TURN_TIMEOUT} s")
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
        f"turn {turn} running; {restart_line}; ulei status exit 0, "
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


def check(title: str, probe: Callable[[], str]) -> bool:
    """Runs one check and annotates its result; whether it passed."""
    try:
        message = probe()
    except (Failure, OSError, subprocess.SubprocessError) as e:
        annotate("error", title, str(e))
        return False
    annotate("notice", title, message)
    return True


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


def restart(api: FakeBotApi, ulei: Path, config: Path) -> str:
    restarted = fatal_restart(api)
    status_up(ulei, config)
    return f"{restarted}; ulei status exit 0 again"


def run(mode: str, work: Path) -> bool:
    """Runs every check of ``mode`` whose units started; whether all passed."""
    dirs = layout()
    if mode == "sandbox" and USERNS_SYSCTL.read_text().strip() != "0":
        raise Failure(f"{USERNS_SYSCTL} is {USERNS_SYSCTL.read_text().strip()}, expected 0")
    if dirs["state"].exists():
        shutil.rmtree(dirs["state"])
    qb = work / "qb.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(qb))
    listener.listen()
    fake_qb = FakeQb()
    threading.Thread(target=serve_qb, args=(listener, fake_qb), daemon=True).start()
    api = FakeBotApi(POLL_HOLD_SECONDS)
    threading.Thread(target=api.serve_forever, daemon=True).start()
    try:
        values = settings(dirs, api.server_port, qb)
        config = write_config(dirs, values)
        ulei = dirs["bin"] / "ulei"
        settings_line = host()
        done = systemctl("start", *UNITS)
        print(f"systemctl --user start: exit {done.returncode} {done.stderr.strip()}", flush=True)
        started = [u for u in UNITS if check(
            f"{mode} start {u}", lambda u=u: start(u, settings_line)
        )]
        passed = len(started) == len(UNITS)
        passed &= check(f"{mode} ulei status", lambda: status(ulei, config))
        for unit in started:
            passed &= check(
                f"{mode} sandbox {unit}", lambda u=unit: sandbox(u, values, dirs["home"], mode)
            )
        if mode == "default" and "ulei-tg.service" in started:
            passed &= check("fatal exit of ulei-tg.service", lambda: restart(api, ulei, config))
        if mode == "sandbox" and len(started) == len(UNITS):
            passed &= check(
                "SIGABRT of ulei-engine.service",
                lambda: engine_abort(fake_qb, values, ulei, config),
            )
            passed &= check("SIGABRT of ulei-tg.service", lambda: tg_abort(ulei, config))
        passed &= check(f"{mode} stop", lambda: stop_clean(api))
        return passed
    finally:
        systemctl("stop", *UNITS)
        api.release.set()
        api.shutdown()
        api.server_close()
        listener.close()
        fake_qb.close()


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[0] not in MODES:
        print("usage: units_run.py default|sandbox WORKDIR", file=sys.stderr)
        return 2
    try:
        passed = run(argv[0], Path(argv[1]))
    except (Failure, OSError, subprocess.SubprocessError) as e:
        annotate("error", f"{argv[0]} start test", f"setup failed: {e}")
        return 1
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
