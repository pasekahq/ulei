"""Summary of the sharded cargo-mutants runs of the `assurance` workflow (ADR-0086 E12a).

``python3 ci/mutants_summary.py merge DIR OUT`` reads every ``DIR/*/outcomes.json`` (one
``mutants.out`` per shard, as cargo-mutants writes it), writes ``OUT/missed.txt`` (the names of
the mutants no test caught, one per line, sorted), ``OUT/missed.re`` (one anchored, escaped regex
per name, for ``cargo mutants -F``) and ``OUT/summary.md`` (counts per outcome and per file, the
missed and timed-out mutants), prints the summary into ``$GITHUB_STEP_SUMMARY`` when set and one
``::notice`` line. A shard whose ``outcomes.json`` is missing (cut by the job's time limit) is
named in the summary as not tested: a coverage gap, never a pass.

``python3 ci/mutants_summary.py survivors MISSED OUTCOMES_DIR OUT`` compares the second pass
(the strong tests) with the first: the mutants still missed are the survivors to triage by hand,
written to ``OUT/survivors.txt`` and the summary.
"""

from __future__ import annotations

import json
import os
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path


def outcomes(path: Path) -> list[tuple[str, str]]:
    """(mutant name, summary) of every mutant in one outcomes.json; the baseline is skipped."""
    doc = json.loads(path.read_text(encoding="utf-8"))
    out = []
    for o in doc.get("outcomes", []):
        scenario = o.get("scenario")
        if not isinstance(scenario, dict) or "Mutant" not in scenario:
            continue
        m = scenario["Mutant"]
        name = f"{m.get('file')}:{m.get('span', {}).get('start', {}).get('line')}:" \
               f"{m.get('span', {}).get('start', {}).get('column')}: {m.get('name', m.get('description', ''))}"
        out.append((name, o.get("summary", "Unknown")))
    return out


def names_from_txt(path: Path) -> list[str]:
    return [l.strip() for l in path.read_text(encoding="utf-8").splitlines() if l.strip()] if path.exists() else []


def merge(src: Path, out: Path) -> int:
    out.mkdir(parents=True, exist_ok=True)
    shards = sorted(p for p in src.iterdir() if p.is_dir()) if src.is_dir() else []
    totals: Counter = Counter()
    per_file: dict[str, Counter] = defaultdict(Counter)
    missed, timeouts, gaps = set(), set(), []
    for shard in shards:
        found = list(shard.rglob("outcomes.json"))
        if not found:
            gaps.append(shard.name)
            continue
        base = found[0].parent
        # cargo-mutants' own text lists are the reference names (they match -F).
        missed.update(names_from_txt(base / "missed.txt"))
        timeouts.update(names_from_txt(base / "timeout.txt"))
        for name, summary in outcomes(found[0]):
            totals[summary] += 1
            per_file[name.split(":", 1)[0]][summary] += 1
    (out / "missed.txt").write_text("".join(f"{n}\n" for n in sorted(missed)), encoding="utf-8")
    (out / "missed.re").write_text("".join(f"^{re.escape(n)}$\n" for n in sorted(missed)), encoding="utf-8")
    lines = ["# Mutation testing", "", f"Shards: {len(shards)}, without results: {len(gaps)} {gaps}", "",
             "| Outcome | Mutants |", "|---|---|"]
    lines += [f"| {k} | {v} |" for k, v in sorted(totals.items())]
    lines += ["", "| File | caught | missed | timeout | unviable |", "|---|---|---|---|---|"]
    for f, c in sorted(per_file.items()):
        lines.append(f"| `{f}` | {c['CaughtMutant']} | {c['MissedMutant']} | {c['Timeout']} | {c['Unviable']} |")
    lines += ["", f"## Missed ({len(missed)})", ""] + [f"- `{n}`" for n in sorted(missed)]
    lines += ["", f"## Timed out ({len(timeouts)})", ""] + [f"- `{n}`" for n in sorted(timeouts)]
    text = "\n".join(lines) + "\n"
    (out / "summary.md").write_text(text, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(text)
    print(f"::notice title=mutants::{sum(totals.values())} mutants, {len(missed)} missed, "
          f"{len(timeouts)} timed out, {len(gaps)} shards without results")
    return 0


def survivors(missed_file: Path, second: Path, out: Path) -> int:
    out.mkdir(parents=True, exist_ok=True)
    first = set(names_from_txt(missed_file))
    again = set()
    for p in second.rglob("missed.txt"):
        again.update(names_from_txt(p))
    tested = set()
    for p in second.rglob("outcomes.json"):
        tested.update(n for n, _ in outcomes(p))
    left = sorted(first & again)
    untested = sorted(first - tested - again)
    (out / "survivors.txt").write_text("".join(f"{n}\n" for n in left), encoding="utf-8")
    text = (f"# Survivors after the strong tests\n\n{len(first)} missed in the first pass, "
            f"{len(left)} still missed, {len(untested)} not re-tested (time limit).\n\n"
            + "".join(f"- `{n}`\n" for n in left)
            + ("\n## Not re-tested\n\n" + "".join(f"- `{n}`\n" for n in untested) if untested else ""))
    (out / "survivors.md").write_text(text, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as f:
            f.write(text)
    print(f"::notice title=mutant survivors::{len(left)} of {len(first)} survive the strong tests; "
          f"{len(untested)} not re-tested")
    return 0


def main(argv: list[str]) -> int:
    if len(argv) == 3 and argv[0] == "merge":
        return merge(Path(argv[1]), Path(argv[2]))
    if len(argv) == 4 and argv[0] == "survivors":
        return survivors(Path(argv[1]), Path(argv[2]), Path(argv[3]))
    print("usage: mutants_summary.py merge DIR OUT | survivors MISSED OUTCOMES_DIR OUT", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
