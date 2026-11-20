"""Gitleaks scan of the history of the checked-out commit, and its self-test.

``python3 ci/gitleaks_scan.py GITLEAKS scan`` scans the history of HEAD in the current
directory and exits 1 on a finding. ``python3 ci/gitleaks_scan.py GITLEAKS self-test`` runs
the same invocation in a throwaway repository: a key on HEAD's history must fail the scan, a
key only on another branch must not. Each self-test result is a ``::notice`` line.
"""

from __future__ import annotations

import os
import random
import string
import subprocess
import sys
import tempfile
from pathlib import Path

SCAN_ARGS = ["git", "--redact", "--no-banner", "--exit-code", "1", "--log-opts=HEAD", "."]
GIT_ENV = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}


def scan(gitleaks: str, repo: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run([gitleaks, *SCAN_ARGS], cwd=repo, check=False,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)


def git(repo: Path, *args: str) -> None:
    subprocess.run(["git", "-c", "user.name=self-test", "-c", "user.email=self-test@example.invalid",
                    "-c", "commit.gpgsign=false", *args],
                   cwd=repo, env=GIT_ENV, check=True, stdout=subprocess.DEVNULL)


def commit(repo: Path, name: str, text: str, message: str) -> None:
    (repo / name).write_text(text)
    git(repo, "add", "-A")
    git(repo, "commit", "-q", "-m", message)


def fake_key() -> str:
    # Assembled at run time so no key-shaped literal exists in this repository.
    rng = random.Random(0)
    return "ghp" + "_" + "".join(rng.choice(string.ascii_letters + string.digits) for _ in range(36))


def check(gitleaks: str, repo: Path, case: str, status: int, phrase: str) -> bool:
    result = scan(gitleaks, repo)
    sys.stdout.write(result.stdout)
    ok = result.returncode == status and phrase in result.stdout
    level = "notice" if ok else "error"
    print(f"::{level} title=gitleaks self-test::{case}: exit {result.returncode}, expected {status}",
          flush=True)
    return ok


def self_test(gitleaks: str) -> int:
    line = f"token = {fake_key()}\n"
    with tempfile.TemporaryDirectory() as tmp:
        repo = Path(tmp)
        git(repo, "init", "-q", "-b", "main")
        commit(repo, "README", "clean\n", "clean")
        git(repo, "switch", "-q", "-c", "other")
        commit(repo, "settings.env", line, "key on another branch")
        git(repo, "switch", "-q", "main")
        ok = check(gitleaks, repo, "key only on another branch", 0, "no leaks found")
        commit(repo, "settings.env", line, "key on the checked history")
        commit(repo, "settings.env", "token = unset\n", "key removed from the tree")
        ok = check(gitleaks, repo, "key on the checked commit's history", 1, "leaks found: ") and ok
    return 0 if ok else 1


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in ("scan", "self-test"):
        raise SystemExit("usage: gitleaks_scan.py GITLEAKS scan|self-test")
    gitleaks = str(Path(argv[0]).resolve())
    if argv[1] == "self-test":
        return self_test(gitleaks)
    result = scan(gitleaks, Path.cwd())
    sys.stdout.write(result.stdout)
    return result.returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
