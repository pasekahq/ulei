"""Test of ``deploy/install.sh`` on a release directory and fake homes (ADR-0042 section 10).

``python3 ci/install_run.py RELEASE_DIR WORKDIR`` runs the install script on RELEASE_DIR (the
binaries and SHA256SUMS of ``ci/release.sh``) with HOME under WORKDIR and a ``systemctl`` and
``sudo`` on PATH that record any call. It checks that every binary and unit lands in place with
its mode and bytes and nothing else is written, that ``XDG_CONFIG_HOME`` moves the units, that a
second run rewrites nothing, and that a corrupted binary is refused before any copy. Each check's
result is one GitHub annotation; any failed check makes the exit status 1.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Callable

from units_run import annotate

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "deploy/install.sh"
UNIT_DIR = ROOT / "deploy/systemd"
BINS = ("ulei", "ulei-inbox", "ulei-engine", "ulei-web", "ulei-tg")
UNITS = ("ulei-engine.service", "ulei-inbox.service", "ulei-tg.service")
TIMEOUT = 60.0


class Failure(Exception):
    pass


def snapshot(home: Path) -> dict[str, tuple[str, int, int, int]]:
    """Every file under ``home`` by relative path: its SHA-256, mode, inode and mtime."""
    found = {}
    for path in sorted(home.rglob("*")):
        st = path.lstat()
        if path.is_dir() and not path.is_symlink():
            continue
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        mode = st.st_mode & 0o7777
        found[str(path.relative_to(home))] = (digest, mode, st.st_ino, st.st_mtime_ns)
    return found


def fake_tools(work: Path) -> tuple[Path, Path]:
    """A directory with a ``systemctl`` and a ``sudo`` that log their arguments and fail."""
    bindir = work / "fake-bin"
    bindir.mkdir()
    calls = work / "calls"
    for name in ("systemctl", "sudo"):
        tool = bindir / name
        tool.write_text(f'#!/bin/sh\necho "{name} $*" >> "{calls}"\nexit 1\n')
        tool.chmod(0o755)
    return bindir, calls


class Installer:
    def __init__(self, work: Path) -> None:
        self.bindir, self.calls = fake_tools(work)

    def run(
        self, release: Path, home: Path, config: Path | None = None
    ) -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if k != "XDG_CONFIG_HOME"}
        env["HOME"] = str(home)
        env["PATH"] = f"{self.bindir}{os.pathsep}{env.get('PATH', '')}"
        if config is not None:
            env["XDG_CONFIG_HOME"] = str(config)
        done = subprocess.run(
            ["sh", str(SCRIPT), str(release)], env=env, capture_output=True, text=True,
            timeout=TIMEOUT, check=False,
        )
        print(f"$ HOME={home} sh {SCRIPT} {release}: exit {done.returncode}")
        print(done.stdout + done.stderr, flush=True)
        if self.calls.exists():
            raise Failure(f"the script called {self.calls.read_text().strip()!r}")
        return done


def expect_installed(
    release: Path, home: Path, units: Path, done: subprocess.CompletedProcess
) -> None:
    """Exit 0, the binaries in ``home/.local/bin`` and the units in ``units``, nothing else."""
    if done.returncode != 0:
        raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
    want = {}
    for bin in BINS:
        want[home / ".local/bin" / bin] = ((release / bin).read_bytes(), 0o755)
    for unit in UNITS:
        want[units / unit] = ((UNIT_DIR / unit).read_bytes(), 0o644)
    roots = {home, units}
    written = {p for root in roots if root.exists() for p in root.rglob("*") if not p.is_dir()}
    if written != set(want):
        raise Failure(f"wrote {sorted(map(str, written ^ set(want)))} beyond the expected files")
    for path, (data, mode) in want.items():
        if path.is_symlink() or path.read_bytes() != data:
            raise Failure(f"{path} differs from its source")
        if path.stat().st_mode & 0o7777 != mode:
            raise Failure(f"{path} has mode {path.stat().st_mode & 0o7777:o}, not {mode:o}")
    for command in ("systemctl --user daemon-reload", "systemctl --user enable --now"):
        if command not in done.stdout:
            raise Failure(f"the output does not print {command!r}")


def check(title: str, body: Callable[[], str]) -> bool:
    try:
        message = body()
    except (Failure, OSError, subprocess.SubprocessError) as e:
        annotate("error", title, str(e))
        return False
    annotate("notice", title, message)
    return True


def run(release: Path, work: Path) -> bool:
    work.mkdir(parents=True, exist_ok=True)
    installer = Installer(work)
    home = work / "home"
    home.mkdir()

    def install() -> str:
        done = installer.run(release, home)
        expect_installed(release, home, home / ".config/systemd/user", done)
        return ("5 binaries with mode 0755 in ~/.local/bin, the 3 units unchanged with mode 0644 "
                "in ~/.config/systemd/user, no other file")

    def config_home() -> str:
        other = work / "home-xdg"
        other.mkdir()
        config = work / "xdg"
        done = installer.run(release, other, config)
        expect_installed(release, other, config / "systemd/user", done)
        return "with XDG_CONFIG_HOME set, the units land in $XDG_CONFIG_HOME/systemd/user"

    def again() -> str:
        before = snapshot(home)
        done = installer.run(release, home)
        if done.returncode != 0:
            raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
        after = snapshot(home)
        if after != before:
            keys = before.keys() | after.keys()
            changed = sorted(k for k in keys if before.get(k) != after.get(k))
            raise Failure(f"the second run changed {changed}")
        return (f"the second run exits 0 and leaves all {len(after)} files with the same bytes, "
                "mode, inode and mtime")

    def corrupt() -> str:
        bad = work / "corrupt"
        shutil.copytree(release, bad)
        target = bad / BINS[-1]
        data = bytearray(target.read_bytes())
        data[-1] ^= 0xFF
        target.write_bytes(bytes(data))
        fresh = work / "home-corrupt"
        fresh.mkdir()
        done = installer.run(bad, fresh)
        if done.returncode == 0:
            raise Failure("exit 0 on a corrupted binary")
        lines = done.stderr.strip().splitlines()
        named = len(lines) == 1 and lines[0].startswith("install.sh: verify: ")
        if not named or BINS[-1] not in lines[0]:
            raise Failure(f"stderr is not one verify line naming {BINS[-1]}: {done.stderr!r}")
        left = sorted(str(p.relative_to(fresh)) for p in fresh.rglob("*"))
        if left:
            raise Failure(f"wrote {left} before refusing")
        return f"exit {done.returncode} with {lines[0]!r}, nothing written"

    results = [
        check("install", install),
        check("install XDG_CONFIG_HOME", config_home),
        check("install idempotent", again),
        check("install corrupted binary", corrupt),
    ]
    return all(results)


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: install_run.py RELEASE_DIR WORKDIR", file=sys.stderr)
        return 2
    return 0 if run(Path(argv[0]), Path(argv[1])) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
