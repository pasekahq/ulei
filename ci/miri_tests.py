"""Miri over ``ulei-infer``'s unit tests, one test per process (ADR-0086 E12a).

``python3 ci/miri_tests.py OUT [--filter PREFIX] [--per-test SECONDS] [--jobs N] [-- CARGO_ARGS...]``
lists the lib tests (``cargo miri test ... -- --list``), runs each alone with a time limit and
classifies it: ``passed``, ``ub`` (Miri reported undefined behaviour: "Undefined Behavior"),
``unsupported`` (an operation Miri cannot execute: file ``mmap``, ``mprotect``, inline ``asm!``,
foreign calls), ``failed`` (an ordinary test failure under Miri), ``timeout``. Writes
``OUT/miri.json`` and a table into ``$GITHUB_STEP_SUMMARY``. Exit 1 on any ``ub`` or ``failed``;
``unsupported`` and ``timeout`` are listed, not failures (what Miri cannot reach is named, not
hidden). RUSTFLAGS and MIRIFLAGS come from the environment (the job sets them).
"""

from __future__ import annotations

import concurrent.futures
import json
import os
import subprocess
import sys
import time
from pathlib import Path

CARGO = ["cargo", "miri", "test", "--locked", "-p", "ulei-infer", "--all-features", "--lib"]


def classify(rc: int | None, out: str) -> str:
    if rc is None:
        return "timeout"
    if "Undefined Behavior" in out:
        return "ub"
    if rc == 0:
        return "passed"
    if "unsupported operation" in out or "can't call foreign function" in out or "inline assembly" in out:
        return "unsupported"
    return "failed"


def run_one(name: str, extra: list[str], seconds: int, logs: Path) -> dict:
    t0 = time.time()
    try:
        p = subprocess.run(CARGO + extra + ["--", "--exact", name], capture_output=True, text=True,
                           timeout=seconds, check=False)
        rc, out = p.returncode, p.stdout + p.stderr
    except subprocess.TimeoutExpired as e:
        rc, out = None, (e.stdout or b"").decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
    (logs / (name.replace("::", "__") + ".log")).write_text(out, encoding="utf-8")
    verdict = classify(rc, out)
    first = next((l for l in out.splitlines() if "Undefined Behavior" in l or "unsupported" in l), "")
    return {"test": name, "verdict": verdict, "seconds": round(time.time() - t0, 1), "line": first[:300]}


def main(argv: list[str]) -> int:
    if not argv:
        print(__doc__, file=sys.stderr)
        return 2
    out = Path(argv[0])
    rest = argv[1:]
    extra: list[str] = []
    if "--" in rest:
        k = rest.index("--")
        rest, extra = rest[:k], rest[k + 1:]
    opts = {"--filter": "", "--per-test": "900", "--jobs": "2"}
    for i in range(0, len(rest), 2):
        opts[rest[i]] = rest[i + 1]
    out.mkdir(parents=True, exist_ok=True)
    logs = out / "logs"
    logs.mkdir(exist_ok=True)
    listed = subprocess.run(CARGO + extra + ["--", "--list", "--format", "terse"], capture_output=True,
                            text=True, check=False)
    names = [l[: -len(": test")] for l in listed.stdout.splitlines() if l.endswith(": test")]
    names = [n for n in names if n.startswith(opts["--filter"])]
    if not names:
        print(f"::error title=miri::no test listed: {listed.stderr[-500:]}")
        return 1
    with concurrent.futures.ThreadPoolExecutor(int(opts["--jobs"])) as pool:
        rows = list(pool.map(lambda n: run_one(n, extra, int(opts["--per-test"]), logs), names))
    (out / "miri.json").write_text(json.dumps(rows, indent=1), encoding="utf-8")
    counts: dict[str, int] = {}
    for r in rows:
        counts[r["verdict"]] = counts.get(r["verdict"], 0) + 1
    table = ["| Test | Verdict | s | First line |", "|---|---|---|---|"]
    table += [f"| `{r['test']}` | {r['verdict']} | {r['seconds']} | {r['line'].replace('|', '/')} |" for r in rows]
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(f"# Miri {os.environ.get('MIRI_PASS', '')}\n\n{counts}\n\n" + "\n".join(table) + "\n")
    print(f"::notice title=miri {os.environ.get('MIRI_PASS', '')}::{counts}")
    for r in rows:
        if r["verdict"] in ("ub", "failed"):
            print(f"::error title=miri {r['verdict']}::{r['test']}: {r['line']}")
    return 1 if counts.get("ub") or counts.get("failed") else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
