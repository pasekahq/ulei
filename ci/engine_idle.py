"""Idle driver of the `ulei-engine` budgets in ``ci/budgets.sh`` (ADR-0010 section 4).

``python3 ci/engine_idle.py ULEI_ENGINE WORKDIR`` runs ``ULEI_ENGINE run`` against a fake
gateway that reads the turn request and then stays silent, measures the blocked process,
kills it and prints one ``<name> <value>`` line per measurement.
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from inbox_load import Failure, proc_field

IDLE_SECONDS = 10
SETTLE_SECONDS = 1.0
REQUEST_TIMEOUT = 10.0
POLL_SECONDS = 0.05
# Each wins over the config file or starts the unit discovery, so an inherited value would
# point the run elsewhere.
SETTING_ENVS = (
    "ULEI_QB_SOCKET",
    "ULEI_ENGINE_JOURNAL",
    "ULEI_ENGINE_TERMINAL_OUTPUT",
    "ULEI_ENGINE_SERVICE_UNIT",
    "ULEI_ENGINE_SOUL_FILE",
    "ULEI_ENGINE_INDEX_FILE",
    "ULEI_ENGINE_MEMORY_FILE",
    "ULEI_ENGINE_USER_FILE",
    "ULEI_ENGINE_MEMORY_CHAR_LIMIT",
    "ULEI_ENGINE_USER_CHAR_LIMIT",
    "ULEI_ENGINE_TIMEZONE",
    "ULEI_ENGINE_SKILLS_DIR",
    "ULEI_ENGINE_DISABLED_SKILLS",
    "INVOCATION_ID",
)
# Synthetic prompt files of the byte sizes the deployment's SOUL.md, MEMORY.md and USER.md have,
# so the measured turn carries a system prompt of the deployed size.
BRAIN_SIZES = {"SOUL.md": 4000, "MEMORY.md": 3800, "USER.md": 500}
# A synthetic skills directory of the deployment's shape: skills over categories, the
# categories with a DESCRIPTION.md, the usual and the largest SKILL.md size in bytes.
SKILL_COUNT = 92
SKILL_CATEGORIES = 17
DESCRIBED_CATEGORIES = 16
SKILL_BYTES = 10_000
LARGEST_SKILL_BYTES = 72_000


def exited(engine: subprocess.Popen, when: str) -> Failure:
    _, stderr = engine.communicate()
    line = stderr.decode(errors="replace").strip() or "no stderr"
    return Failure(f"engine exited with status {engine.returncode} {when}: {line}")


def accept(listener: socket.socket, engine: subprocess.Popen) -> socket.socket:
    """The engine's connection to the gateway socket."""
    deadline = time.monotonic() + REQUEST_TIMEOUT
    listener.settimeout(POLL_SECONDS)
    while True:
        try:
            conn, _ = listener.accept()
            return conn
        except TimeoutError:
            if engine.poll() is not None:
                raise exited(engine, "before sending the turn request") from None
            if time.monotonic() > deadline:
                raise Failure(f"no gateway connection within {REQUEST_TIMEOUT} s") from None


def read_turn(conn: socket.socket) -> dict:
    """The complete `POST /v1/model/turn` request body; nothing is answered."""
    conn.settimeout(REQUEST_TIMEOUT)
    received = b""
    while b"\r\n\r\n" not in received:
        received += receive(conn, len(received))
    head, body = received.split(b"\r\n\r\n", 1)
    lines = head.split(b"\r\n")
    if not lines[0].startswith(b"POST /v1/model/turn "):
        raise Failure(f"expected a turn request, got {lines[0].decode(errors='replace')}")
    length = None
    for line in lines[1:]:
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            length = int(value.strip())
    if length is None:
        raise Failure("turn request without Content-Length")
    while len(body) < length:
        body += receive(conn, len(head) + 4 + len(body))
    request = json.loads(body)
    if "request_id" not in request:
        raise Failure(f"turn request without request_id: {request}")
    return request


def receive(conn: socket.socket, have: int) -> bytes:
    try:
        chunk = conn.recv(65536)
    except TimeoutError:
        raise Failure(f"turn request incomplete after {have} bytes") from None
    if not chunk:
        raise Failure(f"connection closed after {have} bytes of the turn request")
    return chunk


def idle_field(engine: subprocess.Popen, field: str) -> int:
    """``field`` of the status of the engine, which must still wait for the answer."""
    if engine.poll() is not None:
        raise exited(engine, "while waiting for the answer")
    return proc_field(engine.pid, "status", field)


def brain(work: Path) -> Path:
    """Writes the synthetic prompt files into ``work/brain`` and returns that directory."""
    brain = work / "brain"
    brain.mkdir()
    sentence = "The budget brain holds synthetic notes of a fixed size. "
    for name, size in BRAIN_SIZES.items():
        entries = [(sentence * 4).strip()] * (size // (len(sentence) * 4 + 3) + 1)
        text = "\n\u00a7\n".join(f"{n}. {e}" for n, e in enumerate(entries))
        (brain / name).write_text(text[:size], encoding="utf-8")
    return brain


def skills(work: Path) -> Path:
    """Writes the synthetic skills directory into ``work/skills`` and returns it."""
    root = work / "skills"
    line = "Follow the synthetic budget procedure step by step and check each result.\n"
    for n in range(SKILL_COUNT):
        category = root / f"category-{n % SKILL_CATEGORIES:02d}"
        skill = category / f"budget-skill-{n:03d}"
        skill.mkdir(parents=True)
        frontmatter = (
            f"---\nname: budget-skill-{n:03d}\n"
            f'description: "Synthetic skill {n} of the budget run, described at the length '
            f'the deployed skills use for their index lines and listings."\n'
            "version: 1.0.0\nplatforms: [linux]\n"
            "metadata:\n  budget:\n    tags: [budget, synthetic]\n---\n\n"
        )
        size = LARGEST_SKILL_BYTES if n == 0 else SKILL_BYTES
        body = line * ((size - len(frontmatter)) // len(line))
        (skill / "SKILL.md").write_text(frontmatter + body, encoding="utf-8")
    for n in range(DESCRIBED_CATEGORIES):
        (root / f"category-{n:02d}" / "DESCRIPTION.md").write_text(
            f"---\ndescription: Synthetic category {n} of the budget run.\n---\n",
            encoding="utf-8",
        )
    return root


def measure(engine_bin: str, work: Path) -> dict[str, int | str]:
    path = work / "gateway.sock"
    config = work / "ulei.toml"
    files = brain(work)
    skills_dir = skills(work)
    config.write_text(
        f'[qb]\nsocket = "{path}"\n\n[engine]\njournal = "{work / "state/journal.sqlite3"}"\n'
        f'terminal_output = "{work / "state/terminal-output"}"\n'
        'service_unit = "ulei-engine.service"\n'
        f'soul_file = "{files / "SOUL.md"}"\nindex_file = "{files / "INDEX.md"}"\n'
        f'memory_file = "{files / "MEMORY.md"}"\nuser_file = "{files / "USER.md"}"\n'
        f'skills_dir = "{skills_dir}"\n',
        encoding="utf-8",
    )
    env = {k: v for k, v in os.environ.items() if k not in SETTING_ENVS}
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(path))
        listener.listen(1)
        engine = subprocess.Popen(
            [engine_bin, "run", "--session", "budget", "--route", "budget", "--new",
             "--prompt", "Budget idle turn.", "--config", str(config)],
            cwd=work, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        try:
            with accept(listener, engine) as conn:
                request = read_turn(conn)
                if "<available_skills>" not in str(request.get("system")):
                    raise Failure("the turn request carries no skills index")
                time.sleep(SETTLE_SECONDS)
                switches = idle_field(engine, "voluntary_ctxt_switches")
                time.sleep(IDLE_SECONDS)
                switches = idle_field(engine, "voluntary_ctxt_switches") - switches
                hwm = idle_field(engine, "VmHWM")
        finally:
            if engine.returncode is None:
                engine.kill()
                engine.communicate()
    return {
        "request_id": request["request_id"],
        "vmhwm_kb": hwm,
        "idle_wakeups": switches,
        "idle_seconds": IDLE_SECONDS,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: engine_idle.py ULEI_ENGINE WORKDIR", file=sys.stderr)
        return 2
    try:
        with tempfile.TemporaryDirectory(prefix="engine-", dir=argv[1]) as work:
            results = measure(os.path.abspath(argv[0]), Path(work))
    except (Failure, OSError, ValueError) as e:
        print(f"engine_idle: {e}", file=sys.stderr)
        return 1
    for name, value in results.items():
        print(name, value)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
