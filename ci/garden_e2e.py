"""End-to-end scenarios of the garden with the encoder engine (ADR-0087 E12a): the static
musl daemon and helper on the real e5-small weight file, driven over C7 like ci/garden_run.py.

usage:
  garden_e2e.py run --bin DIR --e5 FILE --other FILE [--bge FILE] --texts JSONL --fixtures DIR
                    --work DIR --out DIR [--runner ci|machine] [--seed N] NAME...
  garden_e2e.py fixture --old-bin DIR --onnx-model DIR --ort LIB --texts JSONL --out DIR [--facts N]

NAME: index_query, index_query_1s, helper_kills, daemon_kills, corrupt_inplace, truncate_inplace, replace_model,
unusable_while_running, start_states, upgrade, disk_full, memory_pressure, or all. Each writes OUT/NAME.json with every
check (value, limit, pass); exit 1 if any scenario fails. ``--runner ci`` (the `assurance`
workflow) makes the latency checks informational: a shared runner's timing is not the 1 GB
machine's, which the BitLaunch run measures. ``disk_full`` mounts a loop device and
``memory_pressure`` uses ``systemd-run --scope``: run as root (the workflow uses sudo).

BIN holds ulei-garden and ulei-garden-embed (static musl, native). --e5 is the converted e5-small
file (ci/encoder_model.py), --other a second weight file of another digest and width (the CI tiny
model of ci/garden_encoder.py, or bge-m3) for replace_model, --bge (optional) bge-m3 for
memory_pressure. The corpus is built from --texts (the reference pack's synthetic texts) by
garden_e2e_lib.build: 6 000 one-line passages, deterministic. ``fixture`` makes the upgrade
fixture: a store of the previous release (OLD_BIN: its musl daemon and its ONNX helper) with
--facts facts embedded by the ONNX backend on ONNX_MODEL (ci/garden_onnx.py's tiny model).

Exactness. Vectors are checked byte for byte: after a scenario an independent helper process on
a sound copy of the model embeds every stored passage (entity name, ": ", fact text; the helper
adds the prefix) and every stored vector must equal its bytes; every embeddable fact has exactly
one vector of one model id; the store passes PRAGMA integrity_check and foreign_key_check.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import garden_e2e_lib as C  # noqa: E402

E5, BGE, OTHER = "e5", "bge", "other"
# ADR-0087 section 6 budgets (kB) and ADR-0047's 150 ms deadline of `pointers`.
BUDGET = {
    E5: {"dirty_kb": 60 * 1000, "clean_kb": 100 * 1000, "idle_dirty_kb": 20 * 1000},
    BGE: {"dirty_kb": 150 * 1000, "clean_kb": 350 * 1000, "idle_dirty_kb": 20 * 1000},
}
SHORT_P95_MS = 75.0      # "well under" the 150 ms target: half of it
SHORT_MAX_MS = 150.0
POINTERS_P95_MS = 150.0
QUIET_S = 2              # worker::QUIET


class Ctx:
    def __init__(self, a):
        self.work, self.out, self.fixtures = a.work.resolve(), a.out.resolve(), a.fixtures.resolve()
        self.seed = a.seed
        self.daemon = a.bin.resolve() / "ulei-garden"
        self.helper = a.bin.resolve() / "ulei-garden-embed"
        self.models = {E5: a.e5.resolve(), OTHER: a.other.resolve(), BGE: a.bge.resolve() if a.bge else None}
        self.big = BGE if a.bge else E5
        self.info = a.runner == "ci"
        texts = [json.loads(l) for l in open(a.texts, encoding="utf-8")]
        self.passages, self.queries = C.build(texts, 6000, 20261126)

    def model_dir(self, name: str, model: str = E5) -> Path:
        d = self.work / name / "model"
        d.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(self.models[model], d / "model.ulei-infer")
        return d

    def sound_dir(self, model: str) -> Path:
        """A copy for the reference helper that no scenario touches."""
        d = self.work / f"sound-{model}"
        if not (d / "model.ulei-infer").exists():
            d.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.models[model], d / "model.ulei-infer")
        return d


# ----------------------------------------------------------------------------- building blocks


def fill(res: C.Result, d: C.Daemon, rows: list[dict]) -> None:
    t0 = time.time()
    bad, secret = 0, 0
    for p in rows:
        answer = C.propose(d.sock, p["entity"], p["text"])
        if answer.get("status") == "rejected" and answer.get("reason") == "secret_value":
            # The edge's secret screen refuses a few synthetic texts that name a token or a
            # passport (ADR-0055); policy, not the embedder's business: counted, not failed.
            secret += 1
        elif answer.get("status") != "seeded":
            bad += 1
    outcomes = C.night(d.sock)
    res.metrics.setdefault("fill", []).append({"facts": len(rows), "seconds": round(time.time() - t0, 1),
                                               "outcomes": outcomes, "refused_as_secret": secret})
    res.check(f"{len(rows)} proposes seeded and accepted (but the edge's secret refusals)",
              bad == 0 and outcomes.get("accepted", 0) == len(rows) - secret,
              {"not_seeded": bad, "refused_as_secret": secret, **outcomes}, "all accepted")


def counts(root: Path, model_prefix: str | None = None) -> tuple[int, int]:
    db = C.store_copy(root, root / "probe.sqlite3")
    facts = len(C.store_facts(db))
    sql = "SELECT count(*) FROM fact_vectors" + (" WHERE model LIKE ?" if model_prefix else "")
    vecs = C.q(db, sql, *( [model_prefix + "%"] if model_prefix else []))[0][0]
    return facts, vecs


class Sampler(threading.Thread):
    """/proc samples of the daemon and its helper every ``period`` s."""

    def __init__(self, d: C.Daemon, period: float = 0.5):
        super().__init__(daemon=True)
        self.d, self.period, self.rows, self.stop_ = d, period, [], threading.Event()

    def run(self) -> None:
        while not self.stop_.is_set():
            pid = self.d.proc.pid if self.d.alive() else None
            self.rows.append({"daemon": C.safe_sample(pid), "helper": C.safe_sample(self.d.helper_pid()),
                              "mem_available_kb": C.meminfo().get("MemAvailable")})
            self.stop_.wait(self.period)

    def peak(self, who: str, key: str) -> int:
        return max((r[who][key] for r in self.rows if r[who]), default=0)


class Load(threading.Thread):
    """A query every ``every`` s (search with short, medium or long text; pointers every
    other), each with its latency, answer flags and errors."""

    def __init__(self, d: C.Daemon, queries: list[dict], every: float, seed: int):
        super().__init__(daemon=True)
        self.d, self.qs, self.every, self.r = d, queries, every, C.rng(seed)
        self.rows, self.stop_ = [], threading.Event()
        self.ops = ("search", "pointers")

    def run(self) -> None:
        n = 0
        while not self.stop_.is_set():
            qy = self.r.choice(self.qs)
            op = self.ops[n % len(self.ops)]
            ans, sec, err = C.timed_search(self.d.sock, op, qy["text"])
            self.rows.append({"t": time.time(), "op": op, "class": qy["tokens_class"], "ms": sec * 1000,
                              "err": err, "lexical_only": bool(ans and ans.get("lexical_only"))})
            n += 1
            self.stop_.wait(self.every)

    def lat(self, op: str, cls: str | None = None) -> list[float]:
        return [x["ms"] for x in self.rows if x["op"] == op and not x["err"] and (cls is None or x["class"] == cls)]


def wait_indexed(root: Path, timeout: float, d: C.Daemon, model_prefix: str | None = "ulei-infer") -> float | None:
    """Seconds until every fact has a vector of the engine's model (`model_prefix`; vectors of
    an older backend left in an upgraded store do not count), None on timeout."""
    t0 = time.time()
    while time.time() - t0 < timeout:
        if not d.alive():
            return None
        f, v = counts(root, model_prefix)
        if f and v >= f:
            return time.time() - t0
        time.sleep(10)
    return None


def reference_check(res: C.Result, ctx: Ctx, root: Path, model: str, label: str = "") -> dict:
    """Every stored vector equals an independent helper's bytes for its passage; every
    embeddable fact has exactly one vector, of the model's id; nothing of another model left."""
    db = C.store_copy(root, root / "final.sqlite3")
    facts = C.store_facts(db)
    vecs = C.store_vectors(db)
    models = {m for m, _ in vecs}
    ref = C.RefHelper(ctx.helper, ctx.sound_dir(model))
    wrong, missing = [], []
    try:
        for seq, text in facts.items():
            got = [(m, v) for (m, f), v in vecs.items() if f == seq]
            if not got:
                missing.append(seq)
                continue
            want = ref.embed("passage", text)
            if any(v[1] != want for _, v in got):
                wrong.append(seq)
    finally:
        ref.close()
    extra = [k for k in vecs if k[1] not in facts]
    res.check(f"{label}every fact has a vector", not missing, len(missing), 0)
    res.check(f"{label}every vector equals the reference bytes", not wrong, {"wrong": len(wrong), "of": len(facts)}, 0)
    res.check(f"{label}one model id in the store", len(models) <= 1, sorted(models), "1")
    res.check(f"{label}no vector without its fact", not extra, len(extra), 0)
    integ = C.integrity(db)
    res.check(f"{label}store integrity", integ["integrity_check"] == "ok" and integ["foreign_key_violations"] == 0,
              integ, "ok, 0")
    return {"facts": len(facts), "vectors": len(vecs), "models": sorted(models), "wrong": wrong[:20],
            "missing": missing[:20]}


def blob_offset(path: Path, want: str = "encoder.layer.0.") -> int:
    """File offset of the first byte of the first int8 blob of layer 0 (used by every input)."""
    with open(path, "rb") as f:
        pre = f.read(64)
        n = struct.unpack("<Q", pre[16:24])[0]
        header = json.loads(f.read(n))
    start = (64 + n + 63) // 64 * 64
    for t in header["tensors"]:
        if t["name"].startswith(want) and t["dtype"] == "i8":
            return start + t["offset"] + 7
    raise C.Failure("no int8 blob of layer 0")


def kill9(pid: int) -> bool:
    """SIGKILL `pid`; False if it had already gone (a helper that fell meanwhile)."""
    try:
        os.kill(pid, 9)
        return True
    except ProcessLookupError:
        return False


def flip(path: Path, at: int) -> int:
    with open(path, "r+b") as f:  # in place: the helper's private read-only mapping sees it
        f.seek(at)
        old = f.read(1)[0]
        f.seek(at)
        f.write(bytes([old ^ 0x80]))
        f.flush()
        os.fsync(f.fileno())
    return old


def new_daemon(ctx: Ctx, name: str, model_dir: Path | None, schedule: dict | None = None,
               prefix: list[str] | None = None, store_from: Path | None = None) -> C.Daemon:
    root = ctx.work / name / "garden"
    C.rm_tree(root)
    root.mkdir(mode=0o700, parents=True)
    if store_from:
        shutil.copytree(store_from, root / "store")
        # An artifact download loses the modes; the daemon wants its store owner-only.
        (root / "store").chmod(0o700)
        for f in (root / "store").iterdir():
            f.chmod(0o600)
    emb = C.embedder_keys(ctx.helper, model_dir, load_timeout=120) if model_dir else None
    cfg = C.write_config(root, emb, schedule)
    return C.Daemon(ctx.daemon, root, cfg, prefix)


# ----------------------------------------------------------------------------- scenarios


def index_query(ctx: Ctx) -> C.Result:
    """3 000 passages (e5): indexing under a query load with latency and memory limits, recall
    against the reference neighbours, 10 min idle (0 wake-ups), every vector exact."""
    res = C.Result("index_query")
    md = ctx.model_dir("index_query")
    d = new_daemon(ctx, "index_query", md, {"verify_interval": 86400})
    res.metrics["ready_s"] = d.start()
    sampler = Sampler(d)
    sampler.start()
    try:
        fill(res, d, ctx.passages[:3000])
        load = Load(d, ctx.queries, every=4.0, seed=ctx.seed)
        t0 = time.time()
        load.start()
        took = wait_indexed(d.root, 3600, d)
        load.stop_.set()
        load.join()
        res.check("indexing completes", took is not None, took, "3600 s")
        res.metrics["index_seconds"] = took
        res.metrics["helper_restarts"] = d.stderr().count("restart")
        short = load.lat("search", "short")
        res.metrics["latency_ms"] = {
            f"{op}_{cls}": {"n": len(load.lat(op, cls)), "p50": C.pct(load.lat(op, cls), 50),
                            "p95": C.pct(load.lat(op, cls), 95), "max": max(load.lat(op, cls), default=None)}
            for op in ("search", "pointers") for cls in ("short", "medium", "long")}
        res.check("short search p95 during indexing", (C.pct(short, 95) or 1e9) <= SHORT_P95_MS,
                  C.pct(short, 95), SHORT_P95_MS, informational=ctx.info)
        res.check("short search max during indexing", max(short, default=1e9) <= SHORT_MAX_MS,
                  max(short, default=None), SHORT_MAX_MS, informational=ctx.info)
        res.check("pointers p95 during indexing (short and medium)",
                  (C.pct(load.lat("pointers", "short") + load.lat("pointers", "medium"), 95) or 1e9) <= POINTERS_P95_MS,
                  C.pct(load.lat("pointers", "short") + load.lat("pointers", "medium"), 95), POINTERS_P95_MS, informational=ctx.info)
        errs = [x for x in load.rows if x["err"]]
        res.check("no failed query", not errs, len(errs), 0)
        res.check("no lexical-only answer while the helper was up", not any(x["lexical_only"] for x in load.rows),
                  sum(x["lexical_only"] for x in load.rows), 0)
        res.check("helper dirty memory while embedding", sampler.peak("helper", "dirty_kb") <= BUDGET[E5]["dirty_kb"],
                  sampler.peak("helper", "dirty_kb"), BUDGET[E5]["dirty_kb"])
        res.check("helper clean memory while embedding", sampler.peak("helper", "clean_kb") <= BUDGET[E5]["clean_kb"],
                  sampler.peak("helper", "clean_kb"), BUDGET[E5]["clean_kb"])
        res.metrics["daemon_peak"] = {k: sampler.peak("daemon", k) for k in ("rss_kb", "dirty_kb", "hwm_kb")}
        res.metrics["indexing_rate_per_s"] = 3000 / took if took else None
        res.metrics["load_seconds"] = round(time.time() - t0, 1)
        # Idle: no request, no work left (the worker reconciles every new fact after embedding
        # it, ADR-0046: wait for that queue too); daemon and helper must not wake (verify is
        # 24 h off).
        t_rec = time.time()
        while time.time() - t_rec < 3600:
            db = C.store_copy(d.root, d.root / "rq.sqlite3")
            if C.q(db, "SELECT count(*) FROM reconcile_queue")[0][0] == 0:
                break
            time.sleep(10)
        res.metrics["reconcile_seconds_after_index"] = round(time.time() - t_rec, 1)
        time.sleep(15)
        hp = d.helper_pid()
        before = (C.proc_sample(d.proc.pid), C.safe_sample(hp))
        time.sleep(600)
        after = (C.proc_sample(d.proc.pid), C.safe_sample(hp))
        res.check("daemon idle wake-ups over 600 s", after[0]["switches"] == before[0]["switches"],
                  after[0]["switches"] - before[0]["switches"], 0)
        if before[1] and after[1]:
            res.check("helper idle wake-ups over 600 s", after[1]["switches"] == before[1]["switches"],
                      after[1]["switches"] - before[1]["switches"], 0)
            res.check("helper dirty memory after 10 min idle", after[1]["dirty_kb"] <= BUDGET[E5]["idle_dirty_kb"],
                      after[1]["dirty_kb"], BUDGET[E5]["idle_dirty_kb"])
        res.metrics["idle"] = {"before": before, "after": after}
        # Recall: quiescent searches against the reference neighbours.
        recall(res, ctx, d, E5)
    finally:
        sampler.stop_.set()
        d.stop()
    res.metrics["memory_samples"] = len(sampler.rows)
    (ctx.out / "index_query.samples.json").write_text(json.dumps(sampler.rows))
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    # Observation, not a check: digest items the worker's reconciliation made with e5 at the
    # default reconcile_min_cosine 0.85 (ADR-0047 says an enabled model needs its own value).
    db = d.root / "final.sqlite3"
    res.metrics["edge_proposal_by_reason"] = C.q(
        db, "SELECT status, reason, count(*) FROM edge_proposal GROUP BY status, reason")
    snap = ctx.work / "base-e5-store"
    C.rm_tree(snap)
    shutil.copytree(d.root / "store", snap)
    return res


def index_query_1s(ctx: Ctx) -> C.Result:
    """F7 (ADR-0087 E12a): indexing under one short search a second, the daemon and its helper
    pinned to one CPU and the driver to the others. All 6 000 passages (e5), so the indexing
    lasts long enough for 300 short searches or more; progress sampled every 10 s, every vector
    exact."""
    res = C.Result("index_query_1s")
    cpus = sorted(os.sched_getaffinity(0))
    if len(cpus) < 2:
        res.check("a CPU for the daemon and one for the driver", False, len(cpus), 2)
        return res
    product, driver = cpus[-1], set(cpus[:-1])
    res.metrics["cpus"] = {"daemon_and_helper": product, "driver": sorted(driver), "nproc": len(cpus)}
    os.sched_setaffinity(0, driver)
    md = ctx.model_dir("index_query_1s")
    d = new_daemon(ctx, "index_query_1s", md, {"verify_interval": 86400}, prefix=["taskset", "-c", str(product)])
    try:
        res.metrics["ready_s"] = d.start()
        rows = ctx.passages
        fill(res, d, rows)
        facts, _ = counts(d.root)
        short = [q for q in ctx.queries if q["tokens_class"] == "short"]
        load = Load(d, short, every=1.0, seed=ctx.seed)
        load.ops = ("search",)
        t0 = time.time()
        load.start()
        progress, took = [], None
        while time.time() - t0 < 3600 and d.alive():
            f, v = counts(d.root, "ulei-infer")
            progress.append([round(time.time() - t0, 1), v])
            if f and v >= f:
                took = time.time() - t0
                break
            time.sleep(10)
        load.stop_.set()
        load.join()
        res.check("indexing completes under one search a second", took is not None, took, "3600 s")
        lat = load.lat("search", "short")
        res.metrics.update({
            "facts": facts, "index_seconds": took, "progress": progress,
            "indexing_rate_per_s": facts / took if took else None,
            "searches": len(load.rows),
            "short_search_ms": {"n": len(lat), "p50": C.pct(lat, 50), "p95": C.pct(lat, 95),
                                "p99": C.pct(lat, 99), "max": max(lat, default=None)},
            "helper_restarts": d.stderr().count("restart"),
        })
        res.check("300 short searches or more during indexing", len(lat) >= 300, len(lat), 300)
        res.check("short search p95 during indexing", (C.pct(lat, 95) or 1e9) <= SHORT_P95_MS,
                  C.pct(lat, 95), SHORT_P95_MS, informational=ctx.info)
        res.check("short search max during indexing", max(lat, default=1e9) <= SHORT_MAX_MS,
                  max(lat, default=None), SHORT_MAX_MS, informational=ctx.info)
        errs = [x for x in load.rows if x["err"]]
        res.check("no failed query", not errs, len(errs), 0)
        res.check("no lexical-only answer while the helper was up", not any(x["lexical_only"] for x in load.rows),
                  sum(x["lexical_only"] for x in load.rows), 0)
        (ctx.out / "index_query_1s.searches.json").write_text(json.dumps(load.rows))
    finally:
        d.stop()
        os.sched_setaffinity(0, set(cpus))
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def recall(res: C.Result, ctx: Ctx, d: C.Daemon, model: str, k: int = 5) -> None:
    """For every query: the reference dense ranking over the stored facts (independent helper,
    exact dot products); the daemon's `search` (k) must contain the dense first, which RRF with
    rrf_k = 0 ranks with a score of at least 1 (search.rs SearchParams::at). Also recall@k of
    the dense top-k inside the answer, as a figure."""
    db = C.store_copy(d.root, d.root / "recall.sqlite3")
    facts = C.store_facts(db)
    ids = dict(C.q(db, "SELECT seq, id FROM facts"))
    ref = C.RefHelper(ctx.helper, ctx.sound_dir(model))
    try:
        pv = {seq: C.floats(ref.embed("passage", t)) for seq, t in facts.items()}
        top1_in, overlap, lat, rows = 0, [], [], []
        qs = [x for x in ctx.queries if x["tokens_class"] != "long"]
        for qy in qs:
            qv = C.floats(ref.embed("query", qy["text"]))
            ranked = sorted(pv, key=lambda s: (-C.dot(qv, pv[s]), ids[s]))
            want = [ids[s] for s in ranked[:k]]
            ans, sec, err = C.timed_search(d.sock, "search", qy["text"], k=k)
            got = [f["fact_id"] for f in (ans or {}).get("facts", [])]
            lat.append(sec * 1000)
            top1_in += want[0] in got
            overlap.append(len(set(want) & set(got)) / k)
            rows.append({"q": qy["i"], "want": want, "got": got, "err": err})
    finally:
        ref.close()
    rate = top1_in / len(qs)
    res.check("dense top-1 inside the daemon's top-k", rate >= 0.99, round(rate, 4), 0.99)
    res.metrics["recall"] = {"queries": len(qs), "k": k, "dense_top1_in_answer": rate,
                             "mean_dense_overlap_at_k": sum(overlap) / len(overlap),
                             "quiet_search_ms": {"p50": C.pct(lat, 50), "p95": C.pct(lat, 95)}}
    (ctx.out / f"{res.scenario}.recall.json").write_text(json.dumps(rows, ensure_ascii=False))


def helper_kills(ctx: Ctx) -> C.Result:
    """kill -9 of the helper at random moments while indexing, and during queries: the daemon
    answers every request (lexical-only while down), brings dense back, loses no vector and
    stores no wrong one."""
    res = C.Result("helper_kills")
    r = C.rng(ctx.seed + 1)
    md = ctx.model_dir("helper_kills")
    d = new_daemon(ctx, "helper_kills", md, {"verify_interval": 86400})
    d.start()
    try:
        fill(res, d, ctx.passages[:800])
        load = Load(d, ctx.queries, every=3.0, seed=ctx.seed + 2)
        load.start()
        kills, back = 0, []
        for _ in range(20):
            time.sleep(r.uniform(3, 15))
            pid = d.helper_pid()
            if pid and kill9(pid):
                kills += 1
                back.append(d.wait_dense("ready", 90))
        # During queries: start a search, kill the helper 0 to 30 ms later.
        during = []
        for _ in range(10):
            d.wait_dense("ready", 90)
            pid = d.helper_pid()
            out = {}
            t = threading.Thread(target=lambda: out.update(zip(("a", "s", "e"),
                                 C.timed_search(d.sock, "search", r.choice(ctx.queries)["text"]))))
            t.start()
            time.sleep(r.uniform(0, 0.03))
            if pid:
                kill9(pid)
            t.join(20)
            during.append({"err": out.get("e"), "ms": (out.get("s") or 0) * 1000,
                           "lexical_only": bool(out.get("a") and out["a"].get("lexical_only"))})
        load.stop_.set()
        load.join()
        res.check("daemon alive after all kills", d.alive(), d.alive(), True)
        res.check("dense back after every kill (90 s)", all(b is not None for b in back),
                  [round(b, 1) if b else None for b in back], "<= 90 s")
        errs = [x for x in load.rows if x["err"]] + [x for x in during if x["err"]]
        res.check("every query answered (200)", not errs, errs[:5], 0)
        res.check("a query killed mid-call answers within the call deadline (5 s) + 1 s",
                  all(x["ms"] <= 6000 for x in during), [round(x["ms"]) for x in during], 6000)
        res.metrics.update({"kills": kills, "during_query": during,
                            "lexical_only_answers": sum(x["lexical_only"] for x in load.rows)})
        took = wait_indexed(d.root, 3600, d)
        res.check("indexing completes after the kills", took is not None, took, 3600)
        skipped = d.stderr().count("skipped")
        res.check("no passage skipped (PASSAGE_TRIES) by external kills", skipped == 0, skipped, 0)
    finally:
        d.stop()
    db = C.store_copy(d.root, d.root / "pf.sqlite3")
    res.metrics["passage_failures"] = C.q(db, "SELECT fact, failures FROM passage_failures")
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def daemon_kills(ctx: Ctx) -> C.Result:
    """kill -9 of the daemon at random moments of proposes, the night's seedbed_done, the
    worker's vector writes; restart each time. At the end the store is consistent: integrity,
    every record processed exactly once, one fact per accepted record, every vector exact."""
    res = C.Result("daemon_kills")
    r = C.rng(ctx.seed + 3)
    md = ctx.model_dir("daemon_kills")
    d = new_daemon(ctx, "daemon_kills", md, {"verify_interval": 86400})
    rows = ctx.passages[3000:4000]
    sent = 0
    kills = []
    for round_ in range(15):
        d.start()
        stop = time.time() + r.uniform(0.3, 8.0)
        what = r.choice(["propose", "night", "index"])
        try:
            while time.time() < stop:
                if what == "propose" and sent < len(rows):
                    C.propose(d.sock, rows[sent]["entity"], rows[sent]["text"])
                    sent += 1
                elif what == "night":
                    C.night(d.sock, batch=25)
                else:
                    time.sleep(0.05)
        except (C.Failure, OSError):
            pass
        d.kill()
        kills.append(what)
        db = C.store_copy(d.root, d.root / f"k{round_}.sqlite3")
        integ = C.integrity(db)
        res.check(f"integrity after kill {round_} ({what})", integ["integrity_check"] == "ok"
                  and integ["foreign_key_violations"] == 0, integ, "ok")
    d.start()
    try:
        # Every row once more: a propose killed after its commit but before its answer is
        # stored once; the repeat must not write a second fact (seedbed dedup, then the
        # night's `duplicate`), so the end state is one fact per row either way.
        repeat: dict = {}
        for p in rows:
            st = C.propose(d.sock, p["entity"], p["text"]).get("status")
            repeat[st] = repeat.get(st, 0) + 1
        res.metrics["repeat_propose"] = repeat
        res.metrics["night_outcomes"] = C.night(d.sock)
        took = wait_indexed(d.root, 3600, d)
        res.check("indexing completes after the kills", took is not None, took, 3600)
    finally:
        d.stop()
    db = C.store_copy(d.root, d.root / "end.sqlite3")
    seeds = C.q(db, "SELECT count(*), sum(processed_at IS NOT NULL) FROM seedbed")[0]
    dup = C.q(db, "SELECT count(*) FROM (SELECT entity, text FROM facts WHERE superseded_at IS NULL "
                  "GROUP BY entity, text HAVING count(*) > 1)")[0][0]
    current = C.q(db, "SELECT count(*) FROM facts WHERE superseded_at IS NULL")[0][0]
    res.check("every seedbed record processed", seeds[0] == seeds[1], seeds, "all")
    res.check("no fact written twice", dup == 0, dup, 0)
    refused = repeat.get("rejected", 0)
    res.check("one current fact per proposed row (but the secret refusals)", current == len(rows) - refused,
              current, len(rows) - refused)
    res.metrics.update({"kills": kills, "proposed": sent, "seedbed": seeds})
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def check_cycle_common(ctx: Ctx, res: C.Result, d: C.Daemon, interval: int) -> tuple[int, dict]:
    """Waits for a passed check after the index is complete; returns its start and the vectors
    then (their bytes, to compare later)."""
    deadline = time.time() + 6 * interval + 120
    while time.time() < deadline:
        db = C.store_copy(d.root, d.root / "chk.sqlite3")
        row = C.q(db, "SELECT passed_at FROM model_checks WHERE id = 1")
        last = C.q(db, "SELECT max(computed_at) FROM fact_vectors")[0][0] or 0
        if row and row[0][0] and row[0][0] > last:
            return row[0][0], C.store_vectors(db)
        time.sleep(3)
    raise C.Failure("no passed check after the index completed")


def corrupt_inplace(ctx: Ctx) -> C.Result:
    """A byte of a layer-0 blob flipped in place while the daemon runs (the helper's mapping
    sees it), vectors computed with it, the periodic check: dense down with the reason, exactly
    the vectors since the last passed check dropped, the earlier ones untouched; the byte put
    back: recovery re-embeds only those; every final vector exact."""
    res = C.Result("corrupt_inplace")
    interval = 20
    md = ctx.model_dir("corrupt_inplace")
    f = md / "model.ulei-infer"
    d = new_daemon(ctx, "corrupt_inplace", md, {"verify_interval": interval})
    d.start()
    try:
        fill(res, d, ctx.passages[:300])
        res.check("first 300 indexed", wait_indexed(d.root, 1800, d) is not None, None, 1800)
        passed_at, before = check_cycle_common(ctx, res, d, interval)
        at = blob_offset(f)
        old = flip(f, at)
        t_corrupt = time.time()
        fill(res, d, ctx.passages[300:400])
        down = d.wait_dense("down", 4 * interval + 60)
        res.check("dense down after the corruption", down is not None, down, 4 * interval + 60)
        h = d.health()
        res.check("health names the corrupted file", "model file corrupted" in h.get("dense_error", ""), h, "dense_error")
        ans = C.call(d.sock, "search", {"q": ctx.queries[0]["text"], "no_miss": True})[0]
        res.check("search lexical-only while down", ans.get("lexical_only") is True, ans.get("lexical_only"), True)
        db = C.store_copy(d.root, d.root / "down.sqlite3")
        now_v = C.store_vectors(db)
        checks = C.q(db, "SELECT passed_at, failed_at, failure FROM model_checks")
        res.check("no vector computed at or after the last passed check remains",
                  all(v[2] < passed_at for v in now_v.values()), len([v for v in now_v.values() if v[2] >= passed_at]), 0)
        kept_same = all(now_v.get(k) == v for k, v in before.items() if v[2] < passed_at)
        res.check("vectors from before the last passed check unchanged", kept_same, None, "byte-equal")
        dropped_line = [l for l in d.stderr().splitlines() if "vectors computed since the last passed check dropped" in l]
        res.check("one stderr line counts the dropped vectors", len(dropped_line) == 1, dropped_line, 1)
        time.sleep(2 * interval)
        n1 = len(C.store_vectors(C.store_copy(d.root, d.root / "down2.sqlite3")))
        res.check("nothing embedded while down", n1 == len(now_v), n1 - len(now_v), 0)
        # Put the byte back in place: the same digest again.
        with open(f, "r+b") as fh:
            fh.seek(at)
            fh.write(bytes([old]))
            fh.flush()
            os.fsync(fh.fileno())
        up = d.wait_dense("ready", 4 * interval + 120)
        res.check("dense ready after the file verifies again", up is not None, up, 4 * interval + 120)
        res.check("indexing completes after recovery", wait_indexed(d.root, 1800, d) is not None, None, 1800)
        res.metrics.update({"passed_at": passed_at, "corrupted_at": t_corrupt, "checks": checks,
                            "dropped_line": dropped_line})
    finally:
        d.stop()
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def truncate_inplace(ctx: Ctx) -> C.Result:
    """The weight file truncated in place by 4 KiB while the helper maps it (the precondition
    weights/map.rs states is broken on purpose): the helper may die of SIGBUS; the daemon must
    survive, go down as corrupted, and recover when a sound file is renamed into place."""
    res = C.Result("truncate_inplace")
    interval = 20
    md = ctx.model_dir("truncate_inplace")
    f = md / "model.ulei-infer"
    d = new_daemon(ctx, "truncate_inplace", md, {"verify_interval": interval})
    d.start()
    try:
        fill(res, d, ctx.passages[:300])
        time.sleep(10)  # indexing under way
        os.truncate(f, f.stat().st_size - 4096)
        load = Load(d, ctx.queries, every=1.0, seed=ctx.seed + 4)
        load.start()
        down = d.wait_dense("down", 4 * interval + 60)
        load.stop_.set()
        load.join()
        res.check("daemon alive", d.alive(), d.alive(), True)
        res.check("dense down", down is not None, down, 4 * interval + 60)
        res.check("every query answered", not [x for x in load.rows if x["err"]],
                  len([x for x in load.rows if x["err"]]), 0)
        tmp = md / "model.ulei-infer.new"
        shutil.copyfile(ctx.models[E5], tmp)
        os.rename(tmp, f)
        up = d.wait_dense("ready", 4 * interval + 120)
        res.check("dense ready after a sound file is renamed in", up is not None, up, 4 * interval + 120)
        res.check("indexing completes", wait_indexed(d.root, 1800, d) is not None, None, 1800)
        res.metrics["stderr_tail"] = d.stderr().splitlines()[-30:]
    finally:
        d.stop()
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def replace_model(ctx: Ctx) -> C.Result:
    """Another model renamed over the running e5 file: the check sees another digest, restarts
    the helper, the model id changes, every fact is embedded again with it (another width) and
    no e5 vector is left; search answers throughout."""
    res = C.Result("replace_model")
    interval = 30
    md = ctx.model_dir("replace_model")
    d = new_daemon(ctx, "replace_model", md, {"verify_interval": interval})
    d.start()
    sampler = Sampler(d)
    try:
        fill(res, d, ctx.passages[:300])
        res.check("e5 index complete", wait_indexed(d.root, 1800, d) is not None, None, 1800)
        tmp = md / "model.ulei-infer.new"
        shutil.copyfile(ctx.models[OTHER], tmp)
        os.rename(tmp, md / "model.ulei-infer")
        sampler.start()
        load = Load(d, ctx.queries, every=4.0, seed=ctx.seed + 5)
        load.start()
        t0 = time.time()
        done = None
        while time.time() - t0 < 3600:
            db = C.store_copy(d.root, d.root / "p.sqlite3")
            dims = C.q(db, "SELECT dim, count(*) FROM fact_vectors GROUP BY dim")
            if len(dims) == 1 and dims[0][0] != 384 and dims[0][1] == len(C.store_facts(db)):
                done = time.time() - t0
                break
            time.sleep(10)
        load.stop_.set()
        load.join()
        res.check("every fact re-embedded with the other model, no e5 vector left", done is not None, done, 3600)
        res.check("every query answered during the switch", not [x for x in load.rows if x["err"]],
                  len([x for x in load.rows if x["err"]]), 0)
        res.metrics["other_helper_peak"] = {k: sampler.peak("helper", k) for k in ("dirty_kb", "clean_kb")}
    finally:
        sampler.stop_.set()
        d.stop()
    res.metrics["reference"] = reference_check(res, ctx, d.root, OTHER)
    return res


def reseal(path: Path) -> None:
    import hashlib
    with open(path, "r+b") as f:
        data = bytearray(f.read())
        data[32:64] = bytes(32)
        data[32:64] = hashlib.sha256(bytes(data)).digest()
        f.seek(0)
        f.write(data)


def unusable_while_running(ctx: Ctx) -> C.Result:
    """The model file replaced by an older format, then removed, under a running helper
    (ADR-0087 E12a, F5): each time dense goes down with the reason and search answers
    lexical-only; no vector is dropped; a sound file renamed back brings dense back."""
    res = C.Result("unusable_while_running")
    interval = 15
    md = ctx.model_dir("unusable")
    f = md / "model.ulei-infer"
    d = new_daemon(ctx, "unusable", md, {"verify_interval": interval})
    d.start()
    try:
        fill(res, d, ctx.passages[:60])
        res.check("indexed", wait_indexed(d.root, 900, d) is not None, None, 900)
        before = len(C.store_vectors(C.store_copy(d.root, d.root / "u0.sqlite3")))
        for case in ("old_format", "missing"):
            tmp = md / "next.uiw"
            if case == "old_format":
                data = bytearray(ctx.models[E5].read_bytes())
                data[8:12] = struct.pack("<I", 2)
                tmp.write_bytes(bytes(data))
                reseal(tmp)
                os.rename(tmp, f)
            else:
                f.unlink()
            down = d.wait_dense("down", 4 * interval + 30)
            h = d.health()
            res.check(f"{case}: dense down", down is not None, down, 4 * interval + 30)
            res.check(f"{case}: health says unusable", "model file unusable" in h.get("dense_error", ""),
                      h.get("dense_error"), "model file unusable")
            a = C.call(d.sock, "search", {"q": ctx.queries[0]["text"], "no_miss": True})[0]
            res.check(f"{case}: search lexical-only", a.get("lexical_only") is True, a.get("lexical_only"), True)
            n = len(C.store_vectors(C.store_copy(d.root, d.root / "u1.sqlite3")))
            res.check(f"{case}: no vector dropped", n == before, n, before)
            shutil.copyfile(ctx.models[E5], tmp)
            os.rename(tmp, f)
            up = d.wait_dense("ready", 4 * interval + 60)
            res.check(f"{case}: ready again after a sound file", up is not None, up, 4 * interval + 60)
    finally:
        d.stop()
    res.metrics["reference"] = reference_check(res, ctx, d.root, E5)
    return res


def start_states(ctx: Ctx) -> C.Result:
    """The daemon's start on a truncated, missing, old-format and old-container file, and on a
    store whose last check failed."""
    res = C.Result("start_states")
    # (a) truncated file: lexical-only start, then recovery when a sound file is renamed in.
    md = ctx.model_dir("start_truncated")
    f = md / "model.ulei-infer"
    os.truncate(f, f.stat().st_size // 2)
    d = new_daemon(ctx, "start_truncated", md, {"verify_interval": 15})
    d.start()
    try:
        h = d.health()
        res.check("truncated: starts with dense down", h.get("dense") == "down", h, "down")
        res.check("truncated: stderr says lexical-only", "starting lexical-only" in d.stderr(), None, "line")
        fill(res, d, ctx.passages[:20])
        a = C.call(d.sock, "search", {"q": ctx.queries[0]["text"], "no_miss": True})[0]
        res.check("truncated: search answers lexical-only", a.get("lexical_only") is True, a.get("lexical_only"), True)
        tmp = md / "x.new"
        shutil.copyfile(ctx.models[E5], tmp)
        os.rename(tmp, f)
        up = d.wait_dense("ready", 120)
        res.check("truncated: recovers after a sound file", up is not None, up, 120)
    finally:
        d.stop()
    # (b) missing, (c) old model format (meta format /3, resealed), (d) container version 2.
    for name, prep, needle in (
        ("missing", lambda p: p.unlink(), "embedder"),
        ("old_format", lambda p: (p.write_bytes(p.read_bytes().replace(b"ulei-infer/bert-int8/4",
                                                                        b"ulei-infer/bert-int8/3", 1)), reseal(p)),
         "convert the model again"),
        ("old_container", lambda p: (p.write_bytes(p.read_bytes()[:8] + struct.pack("<I", 2) + p.read_bytes()[12:]),
                                     reseal(p)), "convert the model again"),
    ):
        md = ctx.model_dir(f"start_{name}")
        prep(md / "model.ulei-infer")
        d = new_daemon(ctx, f"start_{name}", md)
        d.start(expect_ready=False)
        rc = d.wait_exit(60)
        res.check(f"{name}: refuses to start (exit 1)", rc == 1, rc, 1)
        res.check(f"{name}: one stderr line with the reason", needle in d.stderr(), d.stderr()[-300:], needle)
        d.stop()
    # (e) a store whose last check failed, a sound file: lexical-only start, recovery at the
    # first check.
    snap = ctx.work / "base-e5-store"
    if not snap.exists():
        # Without index_query's store in this run: a small indexed one of its own.
        md = ctx.model_dir("start_base")
        d = new_daemon(ctx, "start_base", md, {"verify_interval": 86400})
        d.start()
        try:
            fill(res, d, ctx.passages[:50])
            wait_indexed(d.root, 600, d)
        finally:
            d.stop()
        shutil.copytree(d.root / "store", snap)
    if snap.exists():
        md = ctx.model_dir("start_failed_check")
        d = new_daemon(ctx, "start_failed_check", md, {"verify_interval": 86400}, store_from=snap)
        import sqlite3
        with sqlite3.connect(d.root / "store" / "garden.sqlite3") as c:
            c.execute("INSERT INTO model_checks (id, failed_at, failure) VALUES (1, 1, 'model file corrupted: test') "
                      "ON CONFLICT(id) DO UPDATE SET failed_at = 1, failure = 'model file corrupted: test'")
        d.start()
        try:
            res.check("failed check on record: lexical-only line at start", "starting lexical-only" in d.stderr(),
                      None, "line")
            up = d.wait_dense("ready", 120)
            res.check("failed check on record: ready after the first check", up is not None, up, 120)
        finally:
            d.stop()
    else:
        res.note("base-e5-store missing (run index_query first): case (e) skipped")
    return res


def upgrade(ctx: Ctx) -> C.Result:
    """A store of the previous release (schema 15) with ONNX-backend vectors, opened by the new
    daemon with the native helper: migration 16, identity change, re-embedding completes,
    search answers throughout, every final vector exact."""
    res = C.Result("upgrade")
    for fx in ("upgrade-onnx",):
        src = ctx.fixtures / fx / "store"
        if not src.exists():
            res.note(f"{fx}: fixture missing, skipped")
            continue
        md = ctx.model_dir(f"upgrade_{fx}")
        before = C.integrity(src / "garden.sqlite3")
        old_models = C.q(src / "garden.sqlite3", "SELECT model, dim, count(*) FROM fact_vectors GROUP BY model, dim")
        d = new_daemon(ctx, f"upgrade_{fx}", md, {"verify_interval": 3600}, store_from=src)
        d.start()
        try:
            load = Load(d, ctx.queries, every=2.5, seed=ctx.seed + 6)
            load.start()
            took = wait_indexed(d.root, 3600, d)
            load.stop_.set()
            load.join()
            res.check(f"{fx}: re-embedding completes", took is not None, took, 3600)
            res.check(f"{fx}: every query answered throughout", not [x for x in load.rows if x["err"]],
                      len([x for x in load.rows if x["err"]]), 0)
        finally:
            d.stop()
        after = C.integrity(d.root / "store" / "garden.sqlite3")
        res.check(f"{fx}: schema 15 -> 16", before["user_version"] == 15 and after["user_version"] == 16,
                  [before["user_version"], after["user_version"]], "15 -> 16")
        res.metrics[fx] = {"old_models": old_models,
                           "reference": reference_check(res, ctx, d.root, E5, label=f"{fx}: ")}
    return res


def disk_full(ctx: Ctx) -> C.Result:
    """The store on a 256 MiB ext4 that fills up during indexing and during a check: no crash,
    reads keep working, writes fail as values, everything completes once space is back."""
    res = C.Result("disk_full")
    ctx.work.mkdir(parents=True, exist_ok=True)
    img, mnt = ctx.work / "small.img", Path("/mnt/garden-e2e-small")
    subprocess.run(["truncate", "-s", "256M", str(img)], check=True)
    subprocess.run(["mkfs.ext4", "-q", "-F", str(img)], check=True)
    mnt.mkdir(parents=True, exist_ok=True)
    subprocess.run(["mount", "-o", "loop", str(img), str(mnt)], check=True)
    try:
        md = ctx.model_dir("disk_full")
        root = mnt / "garden"
        emb = C.embedder_keys(ctx.helper, md, load_timeout=120)
        cfg = C.write_config(root, emb, {"verify_interval": 15})
        d = C.Daemon(ctx.daemon, root, cfg)
        d.start()
        try:
            fill(res, d, ctx.passages[:500])
            time.sleep(20)
            filler = mnt / "filler"
            subprocess.run(["fallocate", "-l", str(shutil.disk_usage(mnt).free), str(filler)], check=False)
            with open(mnt / "filler2", "wb") as fh:  # the last blocks fallocate left
                try:
                    while True:
                        fh.write(b"\0" * 65536)
                except OSError:
                    pass
            res.metrics["free_bytes_when_full"] = shutil.disk_usage(mnt).free
            fails, reads = 0, []
            for p in ctx.passages[500:900]:
                try:
                    ans = C.propose(d.sock, p["entity"], p["text"])
                    fails += ans.get("status") != "seeded"
                except (C.Failure, OSError):
                    fails += 1
                reads.append(C.timed_search(d.sock, "search", ctx.queries[0]["text"])[2])
            try:
                res.metrics["night_while_full"] = C.night(d.sock)
            except (C.Failure, OSError) as e:
                res.metrics["night_while_full"] = f"failed: {e}"
            time.sleep(40)  # at least two checks while full
            res.metrics["stderr_while_full"] = d.stderr().splitlines()[-10:]
            res.check("daemon alive with the disk full", d.alive(), d.alive(), True)
            res.check("searches answered with the disk full", not [e for e in reads if e], [e for e in reads if e][:3], 0)
            res.metrics["proposes_failed_while_full"] = fails
            h = d.health()
            res.metrics["health_full"] = h
            filler.unlink(missing_ok=True)
            (mnt / "filler2").unlink(missing_ok=True)
            C.night(d.sock)
            res.check("indexing completes once space is back", wait_indexed(root, 1800, d) is not None, None, 1800)
        finally:
            d.stop()
        res.metrics["reference"] = reference_check(res, ctx, root, E5)
        res.metrics["stderr_tail"] = d.stderr().splitlines()[-30:]
    finally:
        subprocess.run(["umount", str(mnt)], check=False)
    return res


def memory_pressure(ctx: Ctx) -> C.Result:
    """bge-m3 under a cgroup MemoryMax (no swap on the machine): the daemon is never killed, the
    store stays consistent; with room for the helper's anonymous memory, everything completes
    exact and without an OOM kill."""
    res = C.Result("memory_pressure")
    limits = (("512M", True), ("192M", True), ("96M", False)) if ctx.big == BGE else \
        (("192M", True), ("96M", True), ("48M", False))
    for limit, must_complete in limits:
        name = f"mem_{limit}"
        md = ctx.model_dir(name, ctx.big)
        unit = f"garden-e2e-{name}-{int(time.time())}"
        prefix = ["systemd-run", "--scope", "--quiet", f"--unit={unit}", "-p", f"MemoryMax={limit}",
                  "-p", "MemorySwapMax=0", "--"]
        d = new_daemon(ctx, name, md, {"verify_interval": 600}, prefix=prefix)
        # The copy of the weight file just written sits in the page cache, charged to this
        # driver's cgroup: drop it, so the daemon's scope faults (and is charged for) the pages
        # its helper touches (the first CI run saw a 16 MB peak under every limit).
        os.sync()
        try:
            Path("/proc/sys/vm/drop_caches").write_text("3\n")
        except OSError as e:
            res.note(f"drop_caches: {e}")
        d.start(ready_timeout=180)
        try:
            fill(res, d, ctx.passages[:150])
            took = wait_indexed(d.root, 1500, d)
            ev = Path(f"/sys/fs/cgroup/system.slice/{unit}.scope/memory.events")
            events = dict(l.split() for l in ev.read_text().splitlines()) if ev.exists() else {}
            peak = Path(f"/sys/fs/cgroup/system.slice/{unit}.scope/memory.peak")
            res.metrics[name] = {"index_seconds": took, "memory_events": events,
                                 "memory_peak": peak.read_text().strip() if peak.exists() else None,
                                 "restarts_lines": d.stderr().count("restart")}
            res.check(f"{limit}: daemon never killed", d.alive(), d.alive(), True)
            if must_complete:
                res.check(f"{limit}: indexing completes", took is not None, took, 1500)
                res.check(f"{limit}: no OOM kill", int(events.get("oom_kill", 0)) == 0, events.get("oom_kill"), 0)
        finally:
            d.stop()
        if must_complete:
            res.metrics[name]["reference"] = reference_check(res, ctx, d.root, ctx.big, label=f"{limit}: ")
        else:
            integ = C.integrity(C.store_copy(d.root, d.root / "end.sqlite3"))
            res.check(f"{limit}: store consistent", integ["integrity_check"] == "ok", integ, "ok")
    return res


ALL = ["index_query", "index_query_1s", "helper_kills", "daemon_kills", "corrupt_inplace", "truncate_inplace",
       "replace_model", "unusable_while_running", "start_states", "upgrade", "disk_full",
       "memory_pressure"]


def fixture(a) -> int:
    """The upgrade fixture: the previous release's daemon with its ONNX helper fills a store."""
    a.out = a.out.resolve()
    root = a.out / "store-root"
    C.rm_tree(root)
    cfg = C.write_config(root, {"enabled": True, "helper": str(a.old_bin / "ulei-garden-embed"),
                                "model_dir": str(a.onnx_model), "ort_library": str(a.ort),
                                "threads": 2, "load_timeout": 300})
    d = C.Daemon(a.old_bin / "ulei-garden", root, cfg)
    d.start(ready_timeout=300)
    texts = [json.loads(l) for l in open(a.texts, encoding="utf-8")]
    passages, _ = C.build(texts, 6000, 20261126)
    try:
        for p in passages[:a.facts]:
            answer = C.propose(d.sock, p["entity"], p["text"])
            if answer.get("status") != "seeded" and answer.get("reason") != "secret_value":
                raise C.Failure(f"propose: {answer}")
        C.night(d.sock)
        deadline = time.time() + 1800
        while time.time() < deadline:
            db = C.store_copy(root, root / "probe.sqlite3")
            if C.q(db, "SELECT count(*) FROM fact_vectors")[0][0] >= len(C.store_facts(db)) > 0:
                break
            time.sleep(5)
        else:
            raise C.Failure("the old daemon did not embed every fact in 1800 s")
    finally:
        d.stop()
    db = root / "store" / "garden.sqlite3"
    info = {"user_version": C.q(db, "PRAGMA user_version")[0][0], "facts": len(C.store_facts(db)),
            "models": C.q(db, "SELECT model, dim, count(*) FROM fact_vectors GROUP BY model, dim")}
    shutil.copytree(root / "store", a.out / "upgrade-onnx" / "store", dirs_exist_ok=True)
    (a.out / "upgrade-onnx" / "FIXTURE.json").write_text(json.dumps(info, indent=1))
    print(f"::notice title=garden e2e fixture::{info}")
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    for k in ("bin", "e5", "other", "texts", "fixtures", "work", "out"):
        r.add_argument("--" + k, type=Path, required=True)
    r.add_argument("--bge", type=Path)
    r.add_argument("--runner", choices=("ci", "machine"), default="ci")
    r.add_argument("--seed", type=int, default=20261126)
    r.add_argument("names", nargs="+")
    f = sub.add_parser("fixture")
    for k in ("old_bin", "onnx_model", "ort", "texts", "out"):
        f.add_argument("--" + k.replace("_", "-"), dest=k, type=Path, required=True)
    f.add_argument("--facts", type=int, default=300)
    a = ap.parse_args()
    if a.cmd == "fixture":
        return fixture(a)
    ctx = Ctx(a)
    names = ALL if a.names == ["all"] else a.names
    status = 0
    for n in names:
        print(f"== {time.strftime('%FT%TZ', time.gmtime())} {n}", flush=True)
        try:
            res = globals()[n](ctx)
        except Exception as e:  # a harness failure is a failed scenario, with its reason
            res = C.Result(n)
            res.check("scenario ran to the end", False, repr(e), "no exception")
        doc = res.write(ctx.out)
        status |= not doc["pass"]
        failed = [c["name"] for c in doc["checks"] if not c["pass"] and not c["informational"]]
        if failed:
            print(f"::error title=garden e2e {n}::{'; '.join(failed)}")
        else:
            print(f"::notice title=garden e2e {n}::passed {len(doc['checks'])} checks in {doc['seconds']} s")
    return status


if __name__ == "__main__":
    sys.exit(main())
