"""Run the repository's broad local check: fmt, clippy, tests and the architecture checks.

Stdlib only, so any Python 3.9+ interpreter can drive it: ``python3 -m ci.broad_check``.
It mirrors the ``fmt``, ``clippy``, ``test``, ``architecture`` and ``contracts`` CI jobs
(without the base-revision compatibility checks) and exits with the first failing status.
Extra arguments are passed to ``cargo test`` (for example a test-name filter).
Set ``ULEI_CHECK_JOBS`` to limit parallel compile jobs (default 2).

A thin driver for external harnesses; Python here is limited to stdlib CI tools.
All ecosystem code is Rust (ADR-0001).
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def _cargo() -> str:
    found = shutil.which("cargo")
    if found:
        return found
    fallback = Path.home() / ".cargo" / "bin" / "cargo"
    if fallback.is_file() and os.access(fallback, os.X_OK):
        return str(fallback)
    raise SystemExit("broad_check: cargo not found on PATH or in ~/.cargo/bin")


def steps(cargo: str, jobs: str, test_args: list[str]) -> list[list[str]]:
    return [
        [cargo, "fmt", "--all", "--check"],
        [cargo, "clippy", "--workspace", "--all-targets", "--locked", "-j", jobs,
         "--", "-D", "warnings"],
        [cargo, "test", "--workspace", "--locked", "-j", jobs, *test_args],
        [cargo, "xtask", "check"],
    ]


def main(argv: list[str]) -> int:
    cargo = _cargo()
    jobs = os.environ.get("ULEI_CHECK_JOBS", "2")
    env = dict(os.environ)
    bindir = str(Path(cargo).parent)
    env["PATH"] = bindir + os.pathsep + env.get("PATH", "")
    for step in steps(cargo, jobs, argv):
        print("broad_check: " + " ".join(step[1:]), file=sys.stderr, flush=True)
        status = subprocess.run(step, cwd=ROOT, env=env, check=False).returncode
        if status != 0:
            print(f"broad_check: failed with status {status}", file=sys.stderr, flush=True)
            return status
    print("broad_check: ok", file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
