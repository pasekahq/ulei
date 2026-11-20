"""Test of ``deploy/rollback.sh`` on fake homes (docs/runbooks/mvp-pilot-cutover.md, part 3).

``python3 ci/rollback_run.py WORKDIR`` runs the rollback script with HOME under WORKDIR, a
synthetic ``ulei.toml`` and inbox root, and a ``systemctl`` on PATH that records its calls. It
checks the calls in order, the config change, that nothing is deleted, that a second run changes
nothing, that a run with an expected file or unit missing is refused before any change, and that
a failed step is one stderr line naming it. Each check's result is one GitHub annotation; any
failed check makes the exit status 1.
"""

from __future__ import annotations

import hashlib
import os
import subprocess
import sys
from pathlib import Path
from typing import Callable

from units_run import annotate

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "deploy/rollback.sh"
OLD_UNIT = "old-gateway.service"
TIMEOUT = 30.0
STOP = "--user disable --now ulei-tg.service ulei-inbox.service ulei-engine.service"
SHOW = f"--user show -p LoadState --value -- {OLD_UNIT}"
START = f"--user enable --now -- {OLD_UNIT}"
CONFIG = """# Synthetic config of the rollback test.
[inbox]
root = "{home}/live/owner-inbox"
socket = "/run/example/inbox.sock"
deliver = true

[engine]
journal = "{home}/.local/state/ulei/engine/journal.sqlite3"
c6_socket = "/run/example/c6.sock"

[telegram]
token_file = "{home}/secrets/bot.token"
"""

# The fake answers `show` with $FAKE_LOADSTATE and fails any call whose arguments contain
# $FAKE_FAIL.
FAKE_SYSTEMCTL = """#!/bin/sh
echo "$*" >> "{calls}"
case "$*" in
  *"${{FAKE_FAIL:-//}}"*) echo "Failed to $1 $2: synthetic failure" >&2; exit 1 ;;
esac
case "$*" in
  "--user show -p LoadState --value -- "*) echo "${{FAKE_LOADSTATE:-loaded}}" ;;
esac
"""


class Failure(Exception):
    pass


def snapshot(home: Path) -> dict[str, tuple[str, int, int, int]]:
    """Every file under ``home`` by relative path: its SHA-256, mode, inode and mtime."""
    found = {}
    for path in sorted(home.rglob("*")):
        if path.is_dir() and not path.is_symlink():
            continue
        st = path.lstat()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        mode = st.st_mode & 0o7777
        found[str(path.relative_to(home))] = (digest, mode, st.st_ino, st.st_mtime_ns)
    return found


def fake_home(work: Path, name: str) -> Path:
    """A home with the config, an inbox root holding a marker file, a journal and a secret."""
    home = work / name
    root = home / "live/owner-inbox"
    root.mkdir(parents=True, mode=0o700)
    (root / "events.sqlite3").write_bytes(b"synthetic events database\n")
    (root / "producer.key").write_bytes(bytes(range(32)))
    (root / "consumer.lock").write_bytes(b"")
    (root / "MARKER").write_text("synthetic marker\n")
    journal = home / ".local/state/ulei/engine/journal.sqlite3"
    journal.parent.mkdir(parents=True)
    journal.write_bytes(b"synthetic journal\n")
    secret = home / "secrets/bot.token"
    secret.parent.mkdir()
    secret.write_text("synthetic-token-0001\n")
    config = home / ".config/ulei/ulei.toml"
    config.parent.mkdir(parents=True)
    config.write_text(CONFIG.format(home=home))
    for path in (root / "producer.key", secret, config):
        path.chmod(0o600)
    return home


class Rollback:
    def __init__(self, work: Path) -> None:
        self.bindir = work / "fake-bin"
        self.bindir.mkdir()
        self.calls = work / "calls"
        tool = self.bindir / "systemctl"
        tool.write_text(FAKE_SYSTEMCTL.format(calls=self.calls))
        tool.chmod(0o755)

    def run(
        self, home: Path, *args: str, **fake: str
    ) -> tuple[subprocess.CompletedProcess, list[str]]:
        """Runs the script under ``home``; returns its result and the `systemctl` calls."""
        self.calls.unlink(missing_ok=True)
        env = {k: v for k, v in os.environ.items() if k not in ("XDG_CONFIG_HOME", "FAKE_FAIL")}
        env.update(HOME=str(home), PATH=f"{self.bindir}{os.pathsep}{env.get('PATH', '')}", **fake)
        done = subprocess.run(
            ["sh", str(SCRIPT), *args], env=env, capture_output=True, text=True,
            timeout=TIMEOUT, check=False,
        )
        print(f"$ HOME={home} {' '.join(f'{k}={v}' for k, v in fake.items())} sh {SCRIPT} "
              f"{' '.join(args)}: exit {done.returncode}")
        print(done.stdout + done.stderr, flush=True)
        calls = self.calls.read_text().splitlines() if self.calls.exists() else []
        return done, calls


def expect_refused(
    rollback: Rollback, home: Path, step: str, want: str, *args: str, **fake: str
) -> str:
    """Exit 1 with one stderr line of ``step`` naming ``want``, no change, at most `show`."""
    before = snapshot(home)
    done, calls = rollback.run(home, *args, **fake)
    lines = done.stderr.strip().splitlines()
    named = len(lines) == 1 and lines[0].startswith(f"rollback.sh: {step}: ")
    if done.returncode != 1 or not named:
        raise Failure(f"exit {done.returncode}, stderr {done.stderr!r}, not one {step} line")
    if want not in lines[0]:
        raise Failure(f"{lines[0]!r} does not name {want!r}")
    if set(calls) - {SHOW}:
        raise Failure(f"called systemctl {calls} before refusing")
    if snapshot(home) != before:
        raise Failure("changed files before refusing")
    return f"exit 1 with {lines[0]!r}, systemctl calls {calls}, no file changed"


def check(title: str, body: Callable[[], str]) -> bool:
    try:
        message = body()
    except (Failure, OSError, subprocess.SubprocessError) as e:
        annotate("error", title, str(e))
        return False
    annotate("notice", title, message)
    return True


def run(work: Path) -> bool:
    work.mkdir(parents=True, exist_ok=True)
    rollback = Rollback(work)
    home = fake_home(work, "home")
    root = home / "live/owner-inbox"
    config = home / ".config/ulei/ulei.toml"

    def first() -> str:
        before = snapshot(home)
        done, calls = rollback.run(home, OLD_UNIT)
        if done.returncode != 0 or done.stderr:
            raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
        if calls != [SHOW, STOP, START]:
            raise Failure(f"systemctl calls {calls}, not {[SHOW, STOP, START]}")
        want = CONFIG.format(home=home).replace("deliver = true", "deliver = false")
        if config.read_text() != want:
            raise Failure(f"the config reads {config.read_text()!r}, not {want!r}")
        if config.stat().st_mode & 0o7777 != 0o600:
            raise Failure(f"the config has mode {config.stat().st_mode & 0o7777:o}, not 600")
        after = snapshot(home)
        gone = sorted(set(before) - set(after))
        if gone:
            raise Failure(f"deleted {gone}")
        changed = sorted(k for k in before if k != str(config.relative_to(home))
                         and before[k][:2] != after[k][:2])
        if changed:
            raise Failure(f"changed {changed}")
        added = sorted(set(after) - set(before))
        if added != [str((root / "ENABLED").relative_to(home))]:
            raise Failure(f"created {added}, not only the inbox root's ENABLED")
        return (f"systemctl calls in order {calls}; inbox.deliver set to false and nothing else "
                "of the config changed; ENABLED created; the inbox root, its marker, the journal "
                "and the secret kept with their bytes and modes")

    def again() -> str:
        before = snapshot(home)
        done, calls = rollback.run(home, OLD_UNIT)
        if done.returncode != 0 or done.stderr:
            raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
        if calls != [SHOW, STOP, START]:
            raise Failure(f"systemctl calls {calls}, not {[SHOW, STOP, START]}")
        after = snapshot(home)
        if after != before:
            keys = before.keys() | after.keys()
            changed = sorted(k for k in keys if before.get(k) != after.get(k))
            raise Failure(f"the second run changed {changed}")
        return (f"the second run exits 0 with the same calls and leaves all {len(after)} files "
                "with the same bytes, mode, inode and mtime")

    def refused() -> str:
        no_config = fake_home(work, "home-no-config")
        (no_config / ".config/ulei/ulei.toml").unlink()
        no_events = fake_home(work, "home-no-events")
        (no_events / "live/owner-inbox/events.sqlite3").unlink()
        no_unit = fake_home(work, "home-no-unit")
        results = [
            expect_refused(rollback, no_config, "check", "no config", OLD_UNIT),
            expect_refused(rollback, no_events, "check", "no events.sqlite3", OLD_UNIT),
            expect_refused(rollback, no_unit, "check", f"{OLD_UNIT} is not-found", OLD_UNIT,
                           FAKE_LOADSTATE="not-found"),
        ]
        for args in ((), ("ulei-inbox.service",)):
            done, calls = rollback.run(no_unit, *args)
            if done.returncode != 2 or calls:
                raise Failure(
                    f"arguments {args}: exit {done.returncode}, calls {calls}, not a usage error"
                )
        return "; ".join(results) + "; no argument or a ulei unit: exit 2 with no call"

    def failed_step() -> str:
        fresh = fake_home(work, "home-fail")
        done, calls = rollback.run(fresh, OLD_UNIT, FAKE_FAIL="enable --now")
        lines = done.stderr.strip().splitlines()
        named = len(lines) == 1 and lines[0].startswith("rollback.sh: start: ")
        if done.returncode != 1 or not named:
            raise Failure(f"exit {done.returncode}, stderr {done.stderr!r}, not one start line")
        if calls != [SHOW, STOP, START]:
            raise Failure(f"systemctl calls {calls}, not {[SHOW, STOP, START]}")
        return f"a failing start of {OLD_UNIT}: exit 1 with {lines[0]!r}"

    results = [
        check("rollback", first),
        check("rollback idempotent", again),
        check("rollback refused", refused),
        check("rollback failed step", failed_step),
    ]
    return all(results)


def main(argv: list[str]) -> int:
    if len(argv) != 1:
        print("usage: rollback_run.py WORKDIR", file=sys.stderr)
        return 2
    return 0 if run(Path(argv[0])) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
