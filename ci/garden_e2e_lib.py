"""Shared parts of the garden's end-to-end scenarios, ``ci/garden_e2e.py`` (ADR-0086 E12a; stdlib only).

The daemon is driven through the repository's own client tooling: ``ci/inbox_load.py``'s
``call`` (C7 over the Unix socket: one HTTP/1.1 POST per request), ``ci/garden_run.py``'s
``FakeInbox`` (the C1 socket the daemon publishes misses to) and the same config layout as
``ci/garden_run.py``'s ``write_config``, extended by the ``[garden.schedule]`` keys. Its state is
read through C7 ``health`` and, for what C7 does not expose (vectors, checks, schema), read-only
SQLite on a consistent copy of the store.
"""

from __future__ import annotations

import json
import os
import random
import shutil
import signal
import sqlite3
import struct
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from garden_run import FakeInbox  # noqa: E402  (repository tooling)
from inbox_load import Failure, call  # noqa: E402

HELPER_COMM = "ulei-garden-emb"  # comm is cut to 15 bytes


# ----------------------------------------------------------------------------- results


@dataclass
class Result:
    """One scenario's machine-readable result: every check with its value and limit."""

    scenario: str
    checks: list = field(default_factory=list)
    metrics: dict = field(default_factory=dict)
    notes: list = field(default_factory=list)
    started: float = field(default_factory=time.time)

    def check(self, name: str, ok: bool, value=None, limit=None, informational: bool = False) -> bool:
        """One check; an informational one is recorded with its value and limit but does not
        decide the scenario (latency and memory on a shared CI runner, ADR-0086 E12a)."""
        self.checks.append({"name": name, "pass": bool(ok), "value": value, "limit": limit,
                            "informational": informational})
        tag = "PASS" if ok else ("INFO" if informational else "FAIL")
        print(f"[{self.scenario}] {tag} {name}: {value} (limit {limit})", flush=True)
        return ok

    def note(self, text: str) -> None:
        self.notes.append(text)
        print(f"[{self.scenario}] note: {text}", flush=True)

    def write(self, out: Path) -> dict:
        doc = {
            "scenario": self.scenario,
            "pass": bool(self.checks) and all(c["pass"] or c["informational"] for c in self.checks),
            "started": self.started,
            "seconds": round(time.time() - self.started, 1),
            "checks": self.checks,
            "metrics": self.metrics,
            "notes": self.notes,
        }
        out.mkdir(parents=True, exist_ok=True)
        (out / f"{self.scenario}.json").write_text(json.dumps(doc, indent=1, ensure_ascii=False))
        return doc


def pct(values: list[float], p: float) -> float | None:
    if not values:
        return None
    s = sorted(values)
    return s[min(len(s) - 1, int(round(p / 100 * (len(s) - 1))))]


# ----------------------------------------------------------------------------- config / daemon


def write_config(root: Path, embedder: dict | None, schedule: dict | None = None,
                 api: dict | None = None) -> Path:
    """The C9 document of one run under ``root`` (as ci/garden_run.py writes it) with the
    ``[garden.embedder]``, ``[garden.schedule]`` and ``[garden.api]`` keys given."""
    root = root.resolve()
    root.mkdir(mode=0o700, parents=True, exist_ok=True)
    key = root / "producer.key"
    if not key.exists():
        fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            os.write(fd, bytes(range(32)))
        finally:
            os.close(fd)

    def table(name: str, values: dict | None) -> str:
        if not values:
            return ""
        lines = [f"[{name}]"]
        for k, v in values.items():
            if isinstance(v, bool):
                lines.append(f"{k} = {'true' if v else 'false'}")
            elif isinstance(v, int):
                lines.append(f"{k} = {v}")
            else:
                lines.append(f'{k} = "{v}"')
        return "\n".join(lines) + "\n\n"

    # The daemon refuses a socket directory not owner-only (0700).
    (root / "run").mkdir(mode=0o700, exist_ok=True)
    config = root / "ulei.toml"
    config.write_text(
        f'[garden]\nstore = "{root / "store" / "garden.sqlite3"}"\n'
        f'socket = "{root / "run" / "garden.sock"}"\nproducer_key_file = "{key}"\n\n'
        + table("garden.embedder", embedder if embedder else {"enabled": False})
        + table("garden.schedule", schedule)
        + table("garden.api", api)
        + f'[inbox]\nsocket = "{root / "c1.sock"}"\n',
        encoding="utf-8",
    )
    return config


def embedder_keys(helper: Path, model_dir: Path, **extra) -> dict:
    keys = {"enabled": True, "helper": str(helper), "model_dir": str(model_dir),
            "ort_library": str(model_dir / "no-onnxruntime.so"), "threads": 1}
    keys.update(extra)
    return keys


class Daemon:
    """``ulei-garden serve`` under test: started with its stderr in ``root/stderr-N.log``."""

    def __init__(self, binary: Path, root: Path, config: Path, prefix: list[str] | None = None):
        self.binary, self.root, self.config = binary, root, config
        self.prefix = prefix or []
        self.proc: subprocess.Popen | None = None
        self.starts = 0
        self.inbox: FakeInbox | None = None

    @property
    def sock(self) -> str:
        return str(self.root / "run" / "garden.sock")

    def start(self, ready_timeout: float = 60.0, expect_ready: bool = True) -> float:
        if self.inbox is None:
            c1 = self.root / "c1.sock"
            if c1.exists():
                c1.unlink()
            self.inbox = FakeInbox(str(c1))
        self.starts += 1
        err = open(self.root / f"stderr-{self.starts}.log", "wb")
        t0 = time.perf_counter()
        self.proc = subprocess.Popen(self.prefix + [str(self.binary), "serve", "--config", str(self.config)],
                                     stdout=subprocess.DEVNULL, stderr=err)
        err.close()
        if not expect_ready:
            return 0.0
        while True:
            if self.proc.poll() is not None:
                raise Failure(f"daemon exited {self.proc.returncode} before ready: {self.stderr()[-400:]}")
            try:
                call(self.sock, "health", {})
                return time.perf_counter() - t0
            except (FileNotFoundError, ConnectionRefusedError):
                if time.perf_counter() - t0 > ready_timeout:
                    raise Failure("no health answer") from None
                time.sleep(0.01)

    def wait_exit(self, timeout: float) -> int | None:
        try:
            return self.proc.wait(timeout) if self.proc else None
        except subprocess.TimeoutExpired:
            return None

    def kill(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.kill()
        if self.inbox:
            self.inbox.close()
            self.inbox = None

    def alive(self) -> bool:
        return self.proc is not None and self.proc.poll() is None

    def stderr(self) -> str:
        return "".join((self.root / f"stderr-{i}.log").read_text(errors="replace")
                       for i in range(1, self.starts + 1) if (self.root / f"stderr-{i}.log").exists())

    def health(self) -> dict:
        return call(self.sock, "health", {})[0]

    def helper_pid(self) -> int | None:
        """The running embedder helper (not a `verify` child)."""
        if not self.alive():
            return None
        for pid in children(self.proc.pid):
            try:
                comm = Path(f"/proc/{pid}/comm").read_text().strip()
                cmd = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            except OSError:
                continue
            if comm == HELPER_COMM and b"verify" not in cmd:
                return pid
        return None

    def wait_dense(self, state: str, timeout: float) -> float | None:
        """Seconds until `health` says ``dense: state``, None on timeout."""
        t0 = time.time()
        while time.time() - t0 < timeout:
            try:
                if self.health().get("dense") == state:
                    return time.time() - t0
            except (Failure, OSError):
                pass
            time.sleep(0.2)
        return None


def children(pid: int) -> list[int]:
    out = []
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            out += [int(c) for c in (task / "children").read_text().split()]
        except OSError:
            pass
    return out


# ----------------------------------------------------------------------------- /proc samples


def smaps(pid: int) -> dict[str, int]:
    """kB fields of /proc/PID/smaps_rollup (Rss, Private_Dirty, ...)."""
    out = {}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()[1:]:
        k, _, v = line.partition(":")
        parts = v.split()
        if parts and parts[0].isdigit():
            out[k.strip()] = int(parts[0])
    return out


def proc_sample(pid: int) -> dict:
    s = smaps(pid)
    status = {k: v.strip() for k, _, v in
              (l.partition(":") for l in Path(f"/proc/{pid}/status").read_text().splitlines())}
    switches = 0
    for task in Path(f"/proc/{pid}/task").iterdir():
        for l in (task / "status").read_text().splitlines():
            if l.startswith("voluntary_ctxt_switches") or l.startswith("nonvoluntary_ctxt_switches"):
                switches += int(l.split()[1])
    return {
        "t": time.time(),
        "rss_kb": s.get("Rss", 0),
        "dirty_kb": s.get("Private_Dirty", 0),
        "anon_kb": s.get("Anonymous", 0),
        "clean_kb": s.get("Rss", 0) - s.get("Private_Dirty", 0) - s.get("Shared_Dirty", 0),
        "hwm_kb": int(status.get("VmHWM", "0 kB").split()[0]),
        "threads": int(status.get("Threads", "0")),
        "fds": len(os.listdir(f"/proc/{pid}/fd")),
        "switches": switches,
    }


def safe_sample(pid: int | None) -> dict | None:
    if pid is None:
        return None
    try:
        return proc_sample(pid)
    except (OSError, ValueError):
        return None


def meminfo() -> dict[str, int]:
    out = {}
    for l in Path("/proc/meminfo").read_text().splitlines():
        k, _, v = l.partition(":")
        out[k] = int(v.split()[0])
    return out


# ----------------------------------------------------------------------------- store


def store_copy(root: Path, dest: Path) -> Path:
    """A consistent copy of the live store through SQLite's backup API (read-only on the source;
    WAL readers do not block the daemon's writer)."""
    src = root / "store" / "garden.sqlite3"
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists():
        dest.unlink()
    with sqlite3.connect(f"file:{src}?mode=ro", uri=True, timeout=30) as s, sqlite3.connect(dest) as d:
        s.backup(d)
    return dest


def q(db: Path, sql: str, *args) -> list[tuple]:
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as c:
        return c.execute(sql, args).fetchall()


def store_facts(db: Path) -> dict[int, str]:
    """seq -> the passage text the worker embeds (store.rs `missing`: name ': ' text), for every
    version that may carry a vector."""
    rows = q(db, "SELECT f.seq, e.name || ': ' || f.text FROM facts f JOIN entities e ON e.id = f.entity "
                 "WHERE f.sensitivity != 'secret' AND (f.superseded_at IS NULL OR f.recorded_at < f.superseded_at)")
    return dict(rows)


def store_vectors(db: Path) -> dict[tuple[str, int], tuple[int, bytes, int]]:
    return {(m, f): (d, v, c) for m, f, d, v, c in
            q(db, "SELECT model, fact, dim, v, computed_at FROM fact_vectors")}


def integrity(db: Path) -> dict:
    return {
        "integrity_check": q(db, "PRAGMA integrity_check")[0][0],
        "foreign_key_violations": len(q(db, "PRAGMA foreign_key_check")),
        "user_version": q(db, "PRAGMA user_version")[0][0],
    }


# ----------------------------------------------------------------------------- reference helper


class RefHelper:
    """A separate ``ulei-garden-embed`` on a model directory, spoken to as the daemon does (one
    request per line, no lanes id: the single-call exchange every helper keeps)."""

    def __init__(self, helper: Path, model_dir: Path):
        self.p = subprocess.Popen([str(helper), "--model-dir", str(model_dir)], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.ready = json.loads(self.p.stdout.readline())
        self.dim = self.ready["dim"]

    def embed(self, kind: str, text: str) -> bytes:
        self.p.stdin.write((json.dumps({"kind": kind, "texts": [text]}) + "\n").encode())
        self.p.stdin.flush()
        head = json.loads(self.p.stdout.readline())
        if "error" in head:
            raise Failure(f"reference helper refused: {head['error']}")
        return self.p.stdout.read(head["count"] * head["dim"] * 4)

    def close(self) -> None:
        self.p.stdin.close()
        self.p.wait(30)


def floats(b: bytes) -> list[float]:
    return list(struct.unpack(f"<{len(b) // 4}f", b))


def dot(a: list[float], b: list[float]) -> float:
    return sum(x * y for x, y in zip(a, b))


# ----------------------------------------------------------------------------- C7 load


def propose(sock: str, entity: str, text: str, source_ref: str = "synthetic e2e material") -> dict:
    return call(sock, "propose", {"fact": text, "entity": entity, "kind": "person",
                                  "source_kind": "owner", "source_ref": source_ref})[0]


def night(sock: str, batch: int = 100) -> dict[str, int]:
    """All unprocessed seedbed records through seedbed_take / seedbed_done; outcome counts."""
    counts: dict[str, int] = {}
    while True:
        taken, _ = call(sock, "seedbed_take", {"limit": batch})
        refs = [r["ref"] for r in taken["records"]]
        if not refs:
            return counts
        done, _ = call(sock, "seedbed_done", {"items": [{"ref": r} for r in refs]})
        for item in done["items"]:
            for o in item.get("outcomes", []):
                counts[o["status"]] = counts.get(o["status"], 0) + 1


def timed_search(sock: str, op: str, text: str, k: int | None = None) -> tuple[dict | None, float, str | None]:
    body = {"q": text, "no_miss": True} if op == "search" else {"q": text}
    if k and op == "search":
        body["k"] = k
    t0 = time.perf_counter()
    try:
        answer, elapsed = call(sock, op, body)
        return answer, elapsed, None
    except (Failure, OSError) as e:
        return None, time.perf_counter() - t0, str(e)


def rng(seed: int) -> random.Random:
    return random.Random(seed)


def rm_tree(p: Path) -> None:
    shutil.rmtree(p, ignore_errors=True)


# ----------------------------------------------------------------------------- corpus


FIRST = ["Vedmira", "Olgerd", "Miroljub", "Zlatina", "Borislav", "Radmila", "Vsevolod", "Ljubava",
         "Svjatomir", "Darina"]
LAST = ["Olhastov", "Breznjakov", "Tumanovec", "Krecetin", "Saburovskij", "Rakitjanskij", "Zemcugov",
        "Velesov", "Gordeevic", "Ladozin", "Mjatlev", "Cernous", "Sipovnik", "Jaskolov", "Dubravin",
        "Kolyvanov", "Ferzikov", "Hmelev", "Cvetaev", "Jurganov"]
PEOPLE = [f"{f} {l}" for f in FIRST for l in LAST]
MAX_CHARS = 700


def lines_of(text: str) -> list[str]:
    return [l.strip(" #") for l in text.split("\n") if l.strip(" #")]


def one_line(parts: list[str]) -> str:
    return " / ".join(parts)[:MAX_CHARS].strip()


def build(texts: list[str], n: int, seed: int) -> tuple[list[dict], list[dict]]:
    originals = [t[len("passage: "):] for t in texts if t.startswith("passage: ")]
    queries = [t[len("query: "):] for t in texts if t.startswith("query: ")]
    seen: set[str] = set()
    out: list[str] = []

    def add(p: str) -> None:
        if p and p not in seen and len(out) < n:
            seen.add(p)
            out.append(p)

    for o in originals:
        add(one_line(lines_of(o)))
    for width in (2, 3, 4):
        for o in originals:
            ls = lines_of(o)
            for i in range(0, max(0, len(ls) - width + 1)):
                add(one_line(ls[i:i + width]))
    r = random.Random(seed)
    pools = [lines_of(o) for o in originals if lines_of(o)]
    guard = 0
    while len(out) < n and guard < 50 * n:
        guard += 1
        a, b = r.sample(range(len(pools)), 2)
        add(one_line([r.choice(pools[a]), r.choice(pools[b])]))
    passages = [{"i": i, "entity": PEOPLE[i % len(PEOPLE)], "text": p} for i, p in enumerate(out)]
    qs = []
    for i, t in enumerate(queries):
        words = len(t.split())
        qs.append({"i": i, "text": t, "tokens_class": "short" if words <= 8 else "medium"})
    # A few long queries (a whole original passage as the query text, cut by the daemon at
    # 4 000 characters) for the latency tail.
    for j, o in enumerate(originals[::24]):
        qs.append({"i": len(queries) + j, "text": one_line(lines_of(o)), "tokens_class": "long"})
    return passages, qs

