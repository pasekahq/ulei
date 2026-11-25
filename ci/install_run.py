"""Test of ``deploy/install.sh`` on a release directory and fake homes (ADR-0042 section 10,
ADR-0056 section 6).

``python3 ci/install_run.py RELEASE_DIR WORKDIR`` runs the install script on RELEASE_DIR (the
files and SHA256SUMS of ``ci/release.sh``) with HOME under WORKDIR and a ``systemctl`` and
``sudo`` on PATH that record any call. It checks that every binary and unit lands in place with
its mode and bytes and no connector, no ``ulei-priv`` (the owner's root step) or other file is
written, that ``XDG_CONFIG_HOME`` moves the
units, that a second run rewrites nothing, and that a corrupted binary is refused before any
copy; with ``--connector test --dir DIR``, that exactly the test connector's files land in
``DIR/test`` with their modes, a second run rewrites nothing, a corrupted connector file is
refused by both forms before any copy, and an unknown name is refused. Each check's result is one
GitHub annotation; any failed check makes the exit status 1.
"""

from __future__ import annotations

import hashlib
import os
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Callable

from annotations import annotate

ROOT = Path(__file__).resolve().parent.parent
SCRIPT = ROOT / "deploy/install.sh"
UNIT_DIR = ROOT / "deploy/systemd"
BINS = (
    "ulei", "ulei-inbox", "ulei-engine", "ulei-web", "ulei-tg", "ulei-connectors", "ulei-memory",
    "ulei-hive-mcp", "ulei-garden", "ulei-garden-embed",
)
UNITS = (
    "ulei-engine.service", "ulei-inbox.service", "ulei-tg.service", "ulei-connectors.service",
    "ulei-memory.service", "ulei-hive-mcp.service", "ulei-garden.service", "ulei-night.service",
    "ulei-night.timer",
)
CONNECTOR = "test"
CONNECTOR_FILES = {"connector.json": 0o644, "ulei-connector-test": 0o755}
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
        self, release: Path, home: Path, config: Path | None = None, args: tuple[str, ...] = ()
    ) -> subprocess.CompletedProcess:
        env = {k: v for k, v in os.environ.items() if k != "XDG_CONFIG_HOME"}
        env["HOME"] = str(home)
        env["PATH"] = f"{self.bindir}{os.pathsep}{env.get('PATH', '')}"
        if config is not None:
            env["XDG_CONFIG_HOME"] = str(config)
        done = subprocess.run(
            ["sh", str(SCRIPT), *args, str(release)], env=env, capture_output=True, text=True,
            timeout=TIMEOUT, check=False,
        )
        print(f"$ HOME={home} sh {SCRIPT} {' '.join(args)} {release}: exit {done.returncode}")
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
    for command in ("systemctl --user daemon-reload", "systemctl --user enable --now",
                    "ulei-priv is not installed here"):
        if command not in done.stdout:
            raise Failure(f"the output does not print {command!r}")


def expect_connector(
    release: Path, home: Path, target: Path, done: subprocess.CompletedProcess
) -> str:
    """Exit 0, exactly the test connector's release files in ``target`` with their modes, nothing
    in ``home``, and next steps naming the ``ulei.toml`` keys; returns the files as measured."""
    if done.returncode != 0:
        raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
    left = sorted(str(p.relative_to(home)) for p in home.rglob("*"))
    if left:
        raise Failure(f"wrote {left} under HOME")
    names = sorted(p.name for p in target.iterdir())
    if names != sorted(CONNECTOR_FILES):
        raise Failure(f"{target} holds {names}, not {sorted(CONNECTOR_FILES)}")
    found = []
    for name, mode in CONNECTOR_FILES.items():
        path = target / name
        if path.is_symlink() or path.read_bytes() != (
            release / "connectors" / CONNECTOR / name
        ).read_bytes():
            raise Failure(f"{path} differs from the release file")
        if path.stat().st_mode & 0o7777 != mode:
            raise Failure(f"{path} has mode {path.stat().st_mode & 0o7777:o}, not {mode:o}")
        found.append(f"{name} {mode:04o} {path.stat().st_size} bytes")
    for key in (f'dir = "{target.parent.resolve()}"', "enabled", "[memory] socket"):
        if key not in done.stdout:
            raise Failure(f"the next steps do not name {key!r}")
    return ", ".join(found)


def unchanged(root: Path, again: Callable[[], subprocess.CompletedProcess]) -> str:
    """Runs ``again`` and fails unless it exits 0 and every file under ``root`` keeps its bytes,
    mode, inode and mtime."""
    before = snapshot(root)
    done = again()
    if done.returncode != 0:
        raise Failure(f"exit {done.returncode}: {done.stderr.strip()}")
    after = snapshot(root)
    if after != before:
        keys = before.keys() | after.keys()
        changed = sorted(k for k in keys if before.get(k) != after.get(k))
        raise Failure(f"the second run changed {changed}")
    return (f"the second run exits 0 and leaves all {len(after)} files with the same bytes, "
            "mode, inode and mtime")


def refused(
    done: subprocess.CompletedProcess, step: str, named: str, home: Path, target: Path | None = None
) -> str:
    """Exit 1 with one stderr line of ``step`` naming ``named``, nothing under ``home`` and no
    ``target``."""
    lines = done.stderr.strip().splitlines()
    one = len(lines) == 1 and lines[0].startswith(f"install.sh: {step}: ")
    if done.returncode != 1 or not one or named not in lines[0]:
        raise Failure(
            f"exit {done.returncode}, stderr is not one {step} line naming {named}: "
            f"{done.stderr!r}"
        )
    left = sorted(str(p.relative_to(home)) for p in home.rglob("*"))
    if target is not None and target.exists():
        left.append(str(target))
    if left:
        raise Failure(f"wrote {left} before refusing")
    return lines[0]


def corrupted(release: Path, bad: Path, file: str) -> None:
    """A copy of ``release`` at ``bad`` with the last byte of ``file`` flipped."""
    shutil.copytree(release, bad)
    target = bad / file
    data = bytearray(target.read_bytes())
    data[-1] ^= 0xFF
    target.write_bytes(bytes(data))


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
    connector_home = work / "home-connector"
    connector_home.mkdir()
    connectors = work / "connectors"
    connector_args = ("--connector", CONNECTOR, "--dir", str(connectors))

    def install() -> str:
        done = installer.run(release, home)
        expect_installed(release, home, home / ".config/systemd/user", done)
        return (f"{len(BINS)} binaries ({', '.join(BINS)}) with mode 0755 in ~/.local/bin, the "
                f"{len(UNITS)} units ({', '.join(UNITS)}) unchanged with mode 0644 in "
                "~/.config/systemd/user, no connector, not the root helper ulei-priv and no "
                "other file")

    def config_home() -> str:
        other = work / "home-xdg"
        other.mkdir()
        config = work / "xdg"
        done = installer.run(release, other, config)
        expect_installed(release, other, config / "systemd/user", done)
        return "with XDG_CONFIG_HOME set, the units land in $XDG_CONFIG_HOME/systemd/user"

    def again() -> str:
        return unchanged(home, lambda: installer.run(release, home))

    def corrupt() -> str:
        bad = work / "corrupt"
        corrupted(release, bad, BINS[-1])
        fresh = work / "home-corrupt"
        fresh.mkdir()
        line = refused(installer.run(bad, fresh), "verify", BINS[-1], fresh)
        return f"exit 1 with {line!r}, nothing written"

    def connector() -> str:
        done = installer.run(release, connector_home, args=connector_args)
        found = expect_connector(release, connector_home, connectors / CONNECTOR, done)
        return (f"--connector {CONNECTOR} --dir DIR puts exactly {found} into DIR/{CONNECTOR} "
                "with the release bytes, nothing under HOME; the next steps name "
                "connectors.dir, connectors.enabled and memory.socket")

    def connector_again() -> str:
        return unchanged(
            connectors, lambda: installer.run(release, connector_home, args=connector_args)
        )

    def connector_corrupt() -> str:
        bad = work / "corrupt-connector"
        file = f"connectors/{CONNECTOR}/ulei-connector-test"
        corrupted(release, bad, file)
        fresh = work / "home-corrupt-connector"
        fresh.mkdir()
        target = work / "connectors-corrupt"
        args = ("--connector", CONNECTOR, "--dir", str(target))
        line = refused(installer.run(bad, fresh, args=args), "verify", file, fresh, target)
        core = refused(installer.run(bad, fresh), "verify", file, fresh)
        return (f"--connector: exit 1 with {line!r}; without it: exit 1 with {core!r}; "
                "nothing written")

    def unknown() -> str:
        fresh = work / "home-nosuch"
        fresh.mkdir()
        target = work / "connectors-nosuch"
        args = ("--connector", "nosuch", "--dir", str(target))
        done = installer.run(release, fresh, args=args)
        line = refused(done, "connector", "nosuch", fresh, target)
        return f"exit 1 with {line!r}, nothing written"

    results = [
        check("install", install),
        check("install XDG_CONFIG_HOME", config_home),
        check("install idempotent", again),
        check("install corrupted binary", corrupt),
        check("install connector", connector),
        check("install connector idempotent", connector_again),
        check("install corrupted connector file", connector_corrupt),
        check("install unknown connector", unknown),
    ]
    return all(results)


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: install_run.py RELEASE_DIR WORKDIR", file=sys.stderr)
        return 2
    return 0 if run(Path(argv[0]), Path(argv[1])) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
