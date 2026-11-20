"""Start test of the systemd user units of ``deploy/systemd`` (ADR-0042 section 9).

``python3 ci/units_run.py WORKDIR`` needs a running user manager with the three units linked and
the release binaries in ``$HOME/.local/bin``. It writes a synthetic ``ulei.toml`` into the
manager's config directory with every written path in the layout of ADR-0042 section 3, serves a
fake qb gateway under WORKDIR and a loopback fake Bot API, and starts the units. It then checks
that ``ulei status`` exits 0, that each unit's ReadWritePaths hold exactly the paths ``ulei.toml``
names for it, that each process runs with its sandbox and cgroup limits applied, that a fatal
exit of ``ulei-tg`` (exit 1 on a revoked token) is restarted, and that a stop is clean. It prints
what it saw; the first failed check ends it with status 1.
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

from inbox_load import Failure
from status_run import ANSWERS, serve
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
START_TIMEOUT = 20.0
STATUS_TIMEOUT = 30.0
RESTART_TIMEOUT = 40.0
POLL_SECONDS = 0.1
POLL_HOLD_SECONDS = 1.0


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


def proc_status(pid: str) -> dict[str, str]:
    lines = Path(f"/proc/{pid}/status").read_text(encoding="utf-8").splitlines()
    return {name: value.strip() for name, _, value in (line.partition(":") for line in lines)}


def check_sandbox(unit: str, home: Path, dirs: list[Path]) -> str:
    """Checks the hardening of ``unit``'s running process; returns what it saw."""
    pid = show(unit, "MainPID")["MainPID"]
    status = proc_status(pid)
    if status["NoNewPrivs"] != "1" or status["Seccomp"] != "2":
        raise Failure(
            f"{unit}: NoNewPrivs {status['NoNewPrivs']}, Seccomp {status['Seccomp']}; "
            "expected 1 and 2 (filter)"
        )
    if os.readlink(f"/proc/{pid}/ns/mnt") == os.readlink("/proc/self/ns/mnt"):
        raise Failure(
            f"{unit}: pid {pid} runs in the host mount namespace, so ProtectSystem=strict, "
            "ProtectHome=read-only, ReadWritePaths and PrivateTmp are not applied"
        )
    table = mounts(pid)
    for path, want in ((Path("/"), True), (home, True), *((d, False) for d in dirs)):
        if read_only(table, path) != want:
            raise Failure(f"{unit}: {path} is {'writable' if want else 'read-only'} in pid {pid}")
    tmp = [entry for entry in table if entry[0] == Path("/tmp")]
    if not tmp or tmp[-1][1] == "/tmp":
        raise Failure(f"{unit}: /tmp of pid {pid} is the host's, PrivateTmp is not applied")
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
        f"pid {pid}: own mount namespace, / and home read-only, {' '.join(map(str, dirs))} "
        f"writable, private /tmp, NoNewPrivs 1, seccomp filter, memory.max {limits['MemoryMax']}, "
        f"pids.max {limits['TasksMax']}"
    )


def fatal_restart(api: FakeBotApi) -> str:
    """Revokes the token once, so `ulei-tg` exits 1, and waits for systemd to restart it."""
    unit = "ulei-tg.service"
    before = show(unit, "MainPID", "NRestarts")
    api.revoke.set()
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
    # ExecMainCode 1 is CLD_EXITED: the process exited rather than died of a signal.
    if (ended["ExecMainCode"], ended["ExecMainStatus"]) != ("1", "1"):
        raise Failure(
            f"{unit} ended with code {ended['ExecMainCode']} status {ended['ExecMainStatus']}, "
            "expected exit status 1"
        )
    return f"pid {before['MainPID']} exited 1, restarted as pid {state['MainPID']}"


def stop_clean() -> None:
    done = systemctl("stop", *UNITS)
    if done.returncode != 0:
        raise Failure(f"systemctl --user stop: {done.stderr.strip()}")
    for unit in UNITS:
        state = show(unit, "ActiveState", "Result")
        if state != {"ActiveState": "inactive", "Result": "success"}:
            raise Failure(f"{unit} after stop: {state}")


def run(work: Path) -> None:
    dirs = layout()
    qb = work / "qb.sock"
    listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    listener.bind(str(qb))
    listener.listen()
    threading.Thread(target=serve, args=(listener, ANSWERS["qb"]), daemon=True).start()
    api = FakeBotApi(POLL_HOLD_SECONDS)
    threading.Thread(target=api.serve_forever, daemon=True).start()
    try:
        values = settings(dirs, api.server_port, qb)
        config = write_config(dirs, values)
        done = systemctl("start", *UNITS)
        if done.returncode != 0:
            raise Failure(f"systemctl --user start: {done.stderr.strip()}")
        for unit in UNITS:
            print(f"{unit}: running as pid {running(unit, START_TIMEOUT)}")
        ulei = dirs["bin"] / "ulei"
        print("ulei status:\n" + status_up(ulei, config), end="")
        for unit in UNITS:
            dirs_rw = writable(unit)
            check_writes(unit, values, dirs_rw)
            print(f"{unit}: {check_sandbox(unit, dirs['home'], dirs_rw)}")
        print(f"ulei-tg.service fatal exit: {fatal_restart(api)}")
        print("ulei status after the restart:\n" + status_up(ulei, config), end="")
        stop_clean()
        print("stop: every unit inactive with result success")
        if api.errors:
            raise Failure("; ".join(api.errors))
    finally:
        systemctl("stop", *UNITS)
        api.release.set()
        api.shutdown()
        api.server_close()
        listener.close()


def main(argv: list[str]) -> int:
    if len(argv) != 1:
        print("usage: units_run.py WORKDIR", file=sys.stderr)
        return 2
    try:
        run(Path(argv[0]))
    except (Failure, OSError, subprocess.SubprocessError) as e:
        print(f"units_run: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
