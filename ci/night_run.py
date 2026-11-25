"""End to end check of the night gardener ``ulei-engine night`` (ADR-0078) on released binaries.

``python3 ci/night_run.py RELEASE_DIR WORKDIR`` runs ``ulei-garden serve`` and ``ulei-memory
serve`` of RELEASE_DIR on fresh stores under WORKDIR, a scripted C4G gateway on a Unix socket
there, and ``ulei-engine night`` of RELEASE_DIR against them, and checks:

- a dry run prints its estimates per stage, calls no model and changes nothing;
- stage a: seedbed records become facts through the model's candidates, a low-confidence one
  goes to the digest, a noise record is dropped, and the seedbed is empty after the run;
- a malformed answer is asked once more and then leaves its record for the next run, which
  processes it;
- stage b: a forget of a fact's evidence queues it and the night closes it, so `search` no longer
  finds it;
- stage c: three synthetic sessions of the archive become candidates and three summaries in the
  run report, tool output is left out, and a rerun sends nothing;
- the spend limit stops a run after the call that reaches it, and the next run sends only the
  rest.

Its passed checks fold into one notice; a failed check is an error annotation of its own.
Synthetic data only.
"""

from __future__ import annotations

import base64
import json
import os
import socket
import subprocess
import sys
import threading
from pathlib import Path

from annotations import Notices, annotate
from engine_idle import STREAM_HEAD
from garden_run import spawn
from inbox_load import Failure, call

SOURCE = "old-runtime"
RUN_TIMEOUT = 120.0
FOLD = "night gardener end to end"
SESSION_AT = 1_700_000_000


class FakeGateway:
    """A C4G gateway that answers every turn from the prompt: in stage a one candidate per
    record of a known text, in stage c one candidate from message 1 and a summary. In
    ``malformed`` mode it answers text that is no JSON. Each turn reports ``input`` tokens."""

    def __init__(self, path: Path) -> None:
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        self.listener.listen()
        self.turns: list[dict] = []
        self.malformed = False
        self.input = 1_000
        self.lock = threading.Lock()
        threading.Thread(target=self.serve, daemon=True).start()

    def serve(self) -> None:
        while True:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            with conn:
                try:
                    self.answer(conn)
                except (OSError, ValueError, KeyError):
                    pass

    def answer(self, conn: socket.socket) -> None:
        conn.settimeout(10)
        received = b""
        while b"\r\n\r\n" not in received:
            chunk = conn.recv(65536)
            if not chunk:
                return
            received += chunk
        head, body = received.split(b"\r\n\r\n", 1)
        length = 0
        for line in head.split(b"\r\n")[1:]:
            name, _, value = line.partition(b":")
            if name.strip().lower() == b"content-length":
                length = int(value)
        while len(body) < length:
            body += conn.recv(65536)
        request = json.loads(body)
        prompt = request["messages"][0]["content"][0]["text"]
        with self.lock:
            self.turns.append(request)
            malformed, tokens = self.malformed, self.input
        if malformed:
            text = "I could not decide."
        elif request["system"].startswith("You sort notes"):
            text = json.dumps(seed_answer(prompt))
        else:
            text = json.dumps(history_answer(prompt))
        events = [
            {"type": "route", "route": {"vendor": "vendor-a", "model": "model-a",
                                        "login_label": "login-a", "lease": "lease-1"}},
            {"type": "text_delta", "text": text},
            {"type": "usage", "usage": {"input": tokens, "output": 100, "cache_read": 0,
                                        "cache_write": 0}},
            {"type": "stop", "reason": "end"},
        ]
        out = STREAM_HEAD
        for item in events:
            line = json.dumps(item).encode() + b"\n"
            out += f"{len(line):x}\r\n".encode() + line + b"\r\n"
        conn.sendall(out + b"0\r\n\r\n")

    def count(self) -> int:
        with self.lock:
            return len(self.turns)

    def close(self) -> None:
        self.listener.close()


# What the scripted model makes of a seedbed text: entity, kind, attribute, confidence.
KNOWN = {
    "Ada Example lives at 9 Sample Road.": ("Ada Example", "person", "address", 0.9),
    "Cafe Moss might close at 18:00.": ("Cafe Moss", "place", "opening_hours", 0.3),
    "Tobin Marsh works at Heron Labs.": ("Tobin Marsh", "person", "employer", 0.9),
}


def seed_answer(prompt: str) -> dict:
    records = []
    for line in prompt.splitlines():
        record = json.loads(line)
        known = KNOWN.get(record["text"])
        candidates = []
        if known:
            entity, kind, attribute, confidence = known
            candidates.append({"entity": entity, "kind": kind, "fact": record["text"],
                               "attribute": attribute, "confidence": confidence})
        records.append({"ref": record["ref"], "candidates": candidates})
    return {"records": records}


def history_answer(prompt: str) -> dict:
    session = next(line[len("Session: "):] for line in prompt.splitlines()
                   if line.startswith("Session: "))
    n = session.rsplit("-", 1)[-1]
    return {
        "summary": f"In session {session} the owner said where Ilsa Varn {n} lives.",
        "candidates": [{"entity": f"Ilsa Varn {n}", "kind": "person",
                        "fact": f"Ilsa Varn {n} lives in Linden Town.", "message": 1,
                        "attribute": "home_city", "confidence": 0.9}],
    }


def write_config(root: Path) -> Path:
    run, state = root / "run", root / "state"
    run.mkdir(mode=0o700)
    state.mkdir(mode=0o700)
    key = root / "producer.key"
    fd = os.open(key, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        os.write(fd, bytes(range(32)))
    finally:
        os.close(fd)
    config = root / "ulei.toml"
    config.write_text(
        f'[inbox]\nsocket = "{run / "inbox.sock"}"\n\n'
        f'[engine]\nc6_socket = "{run / "c6.sock"}"\n\n'
        f'[qb]\nsocket = "{run / "qb.sock"}"\n\n'
        f'[garden]\nstore = "{state / "garden.sqlite3"}"\nsocket = "{run / "garden.sock"}"\n'
        f'producer_key_file = "{key}"\n\n'
        f'[memory]\narchive = "{state / "memory.archive"}"\nsocket = "{run / "memory.sock"}"\n\n'
        f'[night]\nstate = "{state / "night"}"\nsources = "{SOURCE}"\nmax_usd = "5.0"\n',
        encoding="utf-8",
    )
    return config


class Bed:
    def __init__(self, release: Path, root: Path) -> None:
        self.engine = release / "ulei-engine"
        self.root = root
        self.config = write_config(root)
        self.garden_path = str(root / "run" / "garden.sock")
        self.memory_path = str(root / "run" / "memory.sock")
        self.gateway = FakeGateway(root / "run" / "qb.sock")
        self.daemons = []
        self.daemons.append(spawn(str(release / "ulei-garden"), self.config, self.garden_path)[0])
        self.daemons.append(spawn(str(release / "ulei-memory"), self.config, self.memory_path)[0])

    def close(self) -> None:
        for daemon in self.daemons:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        self.gateway.close()

    def night(self, *args: str, env: dict[str, str] | None = None) -> tuple[str, dict | None]:
        """Runs `ulei-engine night`; its stdout and the last line of its run report."""
        environment = {k: v for k, v in os.environ.items() if not k.startswith("ULEI_")}
        environment.update(env or {})
        done = subprocess.run(
            [str(self.engine), "night", "--config", str(self.config), *args],
            capture_output=True, text=True, timeout=RUN_TIMEOUT, env=environment, check=False,
        )
        if done.returncode != 0:
            raise Failure(f"night {' '.join(args)}: exit {done.returncode}: {done.stderr.strip()}")
        if "--dry-run" in args:
            return done.stdout, None
        report = done.stderr.strip().rsplit("; report ", 1)[-1]
        total = json.loads(Path(report).read_text(encoding="utf-8").splitlines()[-1])
        return done.stdout, total

    def garden(self, method: str, body: dict) -> dict:
        return call(self.garden_path, method, body)[0]

    def propose(self, text: str, ref: str) -> None:
        answer = self.garden("propose", {"fact": text, "source_ref": ref, "source_kind": "owner"})
        if answer.get("status") != "seeded":
            raise Failure(f"propose {text!r}: {answer}")

    def facts(self, query: str) -> list[dict]:
        found = self.garden("search", {"q": query, "no_miss": True, "private": True})["facts"]
        return [f for f in found if not f.get("unprocessed")]

    def put_session(self, n: int) -> None:
        def put(external_id: str, who: str, at: int, text: str) -> None:
            body = {"source": SOURCE, "external_id": external_id, "who": who, "at": at,
                    "kind": "text/plain", "text": text,
                    "bytes_b64": base64.b64encode(text.encode()).decode("ascii")}
            answer = call(self.memory_path, "put", body)[0]
            if answer.get("status") != "stored":
                raise Failure(f"put {external_id}: {answer}")
        at = SESSION_AT + n * 1_000
        put(f"s-{n}", "system", at, f"session s-{n}")
        put(f"s-{n}/m-1", "user", at + 1, f"Ilsa Varn {n} lives in Linden Town now.")
        put(f"s-{n}/m-2", "tool", at + 2, f"tool output {n} with secret-looking noise")
        put(f"s-{n}/m-3", "assistant", at + 3, f"Noted, Ilsa Varn {n} is in Linden Town.")


def dry_run(bed: Bed) -> str:
    for n in (1, 2, 3):
        bed.put_session(n)
    bed.propose("Ada Example lives at 9 Sample Road.", "c8:101")
    out, _ = bed.night("--dry-run")
    lines = {line["stage"]: line for line in map(json.loads, out.splitlines())}
    if bed.gateway.count() != 0:
        raise Failure(f"the dry run called the model {bed.gateway.count()} times")
    a, c = lines["a"], lines["c"]
    if (a["items"], a["calls"], c["sessions"], c["messages"], c["calls"]) != (1, 1, 3, 6, 3):
        raise Failure(f"dry run estimates: {lines}")
    if bed.garden("seedbed_take", {})["records"] == []:
        raise Failure("the dry run processed the seedbed")
    if (bed.root / "state" / "night").exists():
        raise Failure("the dry run wrote the state directory")
    return (f"dry run: stage a {a['items']} record in {a['calls']} call, stage c {c['sessions']} "
            f"sessions of {c['messages']} messages, {c['tokens_in']} tokens in, "
            f"usd {c['usd']:.6f}; no model call, nothing written")


def seedbed(bed: Bed) -> str:
    bed.propose("Cafe Moss might close at 18:00.", "c8:102")
    bed.propose("good morning", "c8:103")
    _, total = bed.night("--stage", "a")
    left = bed.garden("seedbed_take", {})["records"]
    if left:
        raise Failure(f"records left after stage a: {left}")
    ada = bed.facts("Ada Example Sample Road")
    if not ada or ada[0]["source_ref"] != "c8:101":
        raise Failure(f"no accepted fact of Ada Example with its evidence: {ada}")
    digest = bed.garden("digest", {})["items"]
    if not any("Cafe Moss" in json.dumps(item) for item in digest):
        raise Failure(f"the low-confidence candidate is not in the digest: {digest}")
    if (total["taken"], total["accepted"], total["digest"], total["calls"]) != (3, 1, 1, 1):
        raise Failure(f"stage a totals: {total}")
    return (f"stage a: 3 records in 1 call, {total['accepted']} accepted, {total['digest']} into "
            f"the digest, the noise record dropped; tokens {total['tokens_in']}/"
            f"{total['tokens_out']}")


def malformed(bed: Bed) -> str:
    bed.propose("Tobin Marsh works at Heron Labs.", "c8:104")
    before = bed.gateway.count()
    bed.gateway.malformed = True
    _, total = bed.night("--stage", "a")
    bed.gateway.malformed = False
    if bed.gateway.count() - before != 2 or (total["malformed"], total["left"]) != (1, 1):
        raise Failure(f"malformed run: {bed.gateway.count() - before} calls, {total}")
    if len(bed.garden("seedbed_take", {})["records"]) != 1:
        raise Failure("the record of the malformed batch did not wait")
    _, again = bed.night("--stage", "a")
    if again["accepted"] != 1 or not bed.facts("Tobin Marsh Heron"):
        raise Failure(f"the next run did not process the record: {again}")
    return "malformed answer asked twice, its record waited and the next run accepted it"


def forget(bed: Bed) -> str:
    answer = bed.garden("forget", {"refs": ["c8:101"], "reason": "synthetic",
                                   "provenance": "s1-t1"})
    if answer["items"][0]["queue"] != 1:
        raise Failure(f"forget queued {answer}")
    before = bed.gateway.count()
    _, total = bed.night("--stage", "b")
    if (total["forget_rows"], total["cleared"]) != (1, 1) or bed.gateway.count() != before:
        raise Failure(f"stage b: {total}")
    if bed.facts("Ada Example Sample Road"):
        raise Failure("search still finds the forgotten fact")
    return "stage b: the forgotten fact closed without a model call, search no longer finds it"


def history(bed: Bed) -> str:
    before = bed.gateway.count()
    _, total = bed.night("--stage", "c")
    turns = bed.gateway.turns[before:]
    if len(turns) != 3 or (total["sessions"], total["messages"]) != (3, 6):
        raise Failure(f"stage c: {len(turns)} calls, {total}")
    if any("tool output" in t["messages"][0]["content"][0]["text"] for t in turns):
        raise Failure("tool output reached the model")
    for n in (1, 2, 3):
        found = bed.facts(f"Ilsa Varn {n} Linden")
        if not found or not found[0]["source_ref"].startswith("archive:"):
            raise Failure(f"no fact of session {n} with its archive evidence: {found}")
    report = sorted((bed.root / "state" / "night" / "runs").glob("*.jsonl"))[-1]
    sessions = [json.loads(line) for line in report.read_text(encoding="utf-8").splitlines()]
    summaries = [s["summary"] for s in sessions if s.get("type") == "session"]
    if len(summaries) != 3 or not all(s.startswith("In session s-") for s in summaries):
        raise Failure(f"summaries in the report: {summaries}")
    _, again = bed.night("--stage", "c")
    if bed.gateway.count() != before + 3 or again["sessions"] != 0:
        raise Failure(f"a rerun sent {bed.gateway.count() - before - 3} sessions again")
    return (f"stage c: 3 sessions into facts with archive evidence and 3 summaries; the rerun "
            f"sent nothing; tokens {total['tokens_in']}/{total['tokens_out']}")


def spend(bed: Bed) -> str:
    for n in (4, 5, 6):
        bed.put_session(n)
    before = bed.gateway.count()
    # 400,000 input tokens cost 0.4 USD at the default price: the second call starts under the
    # limit of 0.5 and reaches it.
    bed.gateway.input = 400_000
    _, total = bed.night("--stage", "c", env={"ULEI_NIGHT_MAX_USD": "0.5"})
    bed.gateway.input = 1_000
    if total["stop"] != "spend_limit" or total["sessions"] != 2:
        raise Failure(f"spend-limited run: {total}")
    _, rest = bed.night("--stage", "c")
    if rest["sessions"] != 1 or bed.gateway.count() - before != 3:
        raise Failure(f"the next run: {rest}, {bed.gateway.count() - before} calls in all")
    return (f"spend limit 0.5 USD: stopped after 2 sessions at {total['usd']:.4f} USD; the next "
            f"run sent the third only")


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: night_run.py RELEASE_DIR WORKDIR", file=sys.stderr)
        return 2
    release, work = Path(argv[0]).resolve(), Path(argv[1]).resolve()
    root = work / "night"
    root.mkdir(mode=0o700)
    notices = Notices()
    passed = True
    bed = None
    try:
        bed = Bed(release, root)
        for title, probe in (
            ("dry run", dry_run), ("stage a", seedbed), ("malformed answer", malformed),
            ("stage b", forget), ("stage c", history), ("spend limit", spend),
        ):
            ok = notices.check(FOLD, f"night {title}", lambda probe=probe: probe(bed))
            passed &= ok
            if not ok:
                break
    except Failure as e:
        annotate("error", "night gardener setup", str(e))
        passed = False
    finally:
        if bed is not None:
            bed.close()
        notices.write()
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
