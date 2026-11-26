"""Scores a judge of the night's stage d (ADR-0096) on the synthetic judge eval set.

The set, ``ci/reconcile_eval/judge-eval.jsonl``, holds the 419 new facts of the frozen selection
set (``ci/reconcile_eval/gen.py``), each with the earlier facts e5-small picked as its hints in
report E15 and its gold relation to one earlier fact: duplicate, supersedes, contradiction or
none, true by construction. The runner sends the cases as the night does: the system prompt
``crates/engine/src/night/judge.txt``, whole entities per call (``--batch``, default 25), the
same JSON lines and the same strict reading of the answer, asked once more when malformed.

It calls a model, so it never runs in CI. Give it one endpoint:

- ``--gateway SOCKET --route ALIAS``: the qb gateway (C4G) the night uses; qb holds the keys.
- ``--openai-url URL --model NAME``: any OpenAI-compatible chat completions endpoint, the key
  in the environment variable ``JUDGE_EVAL_API_KEY``.

and it prints, and writes as JSON to ``--out``:

- contradiction precision: contradictions on the gold pair of a contradiction case, of all
  contradictions said (a supersedes case called a contradiction is counted apart, as
  confusion, not as an error);
- silent-damage rate: of every pair that is not a gold pair, and of every pair of a ``none``
  case, the share called ``duplicate`` or ``supersedes``, which would retract a fact silently;
- recall per relation on the cases whose gold fact is among the hints (the judge's own recall;
  times the hint recall, 0.886 for e5-small, for end to end);
- the confusion of supersedes and contradiction on gold pairs, the failed calls and the tokens.

``python3 ci/judge_eval.py --self-test`` checks the rendering against the engine's golden file
and the scoring on a scripted judge, without any model. Synthetic data only.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import sys
import time
import urllib.request
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SET = ROOT / "ci" / "reconcile_eval" / "judge-eval.jsonl"
SYSTEM = ROOT / "crates" / "engine" / "src" / "night" / "judge.txt"
GOLDEN = ROOT / "crates" / "engine" / "tests" / "fixtures" / "judge-prompt.txt"
PROMPT_VERSION = "judge-v1"
RELATIONS = ("duplicate", "supersedes", "contradiction")
# The dates the night would show: the hints hold from before, the new fact from after.
EARLIER_FROM, NEW_FROM = "2025-03-01", "2026-11-20"


class Malformed(Exception):
    """The answer breaks the output schema."""


def line(value: dict) -> str:
    """One JSON line as the engine's serde_json writes it: sorted keys, no spaces, UTF-8."""
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def render(batch: list[dict]) -> str:
    """The prompt of one call, as `night::judge::render` makes it: per entity in first-seen
    order one entity line, the union of its cases' hints once as earlier lines, then its new
    facts with the refs of their own hints."""
    out, entities = [], []
    for case in batch:
        if case["entity"]["id"] not in entities:
            entities.append(case["entity"]["id"])
    for entity in entities:
        cases = [c for c in batch if c["entity"]["id"] == entity]
        out.append(line({"entity": {"id": entity, "name": cases[0]["entity"]["name"]}}))
        shown = set()
        for case in cases:
            for hint in case["hints"]:
                if hint["ref"] not in shown:
                    shown.add(hint["ref"])
                    out.append(line({"earlier": {"ref": hint["ref"], "text": hint["text"],
                                                 "valid_from": EARLIER_FROM,
                                                 "source": "owner"}}))
        for case in cases:
            out.append(line({"new": {"ref": case["new"]["ref"], "text": case["new"]["text"],
                                     "valid_from": NEW_FROM, "source": "raw",
                                     "hints": [h["ref"] for h in case["hints"]]}}))
    return "".join(f"{text}\n" for text in out)


def batches(cases: list[dict], per_call: int) -> list[list[dict]]:
    """Whole entities per call, at most `per_call` new facts; a larger entity in parts."""
    groups: dict[str, list[dict]] = {}
    for case in cases:
        groups.setdefault(case["entity"]["id"], []).append(case)
    calls: list[list[dict]] = []
    for group in groups.values():
        for i in range(0, len(group), per_call):
            part = group[i:i + per_call]
            if calls and len(calls[-1]) + len(part) <= per_call:
                calls[-1].extend(part)
            else:
                calls.append(list(part))
    return calls


def parse(text: str, batch: list[dict]) -> dict[str, dict[str, tuple[str, str | None]]]:
    """The verdicts of an answer, by new ref and hint, read as strictly as
    `night::judge::parse`: every new fact once, verdicts only on its own hints, at most one per
    hint, a known relation (`compatible` is dropped), no other key."""
    text = text.strip()
    if text.startswith("```"):
        body = text.split("\n", 1)[1] if "\n" in text else ""
        if not body.rstrip().endswith("```"):
            raise Malformed("unclosed code fence")
        text = body.rstrip()[:-3].strip()
    try:
        root = json.loads(text)
    except ValueError as e:
        raise Malformed("answer is not JSON") from e
    if not isinstance(root, dict) or set(root) != {"items"} or not isinstance(root["items"], list):
        raise Malformed("answer is not an object of items")
    hints = {c["new"]["ref"]: {h["ref"] for h in c["hints"]} for c in batch}
    out: dict[str, dict[str, tuple[str, str | None]]] = {}
    for item in root["items"]:
        if not isinstance(item, dict) or set(item) != {"ref", "verdicts"}:
            raise Malformed("an item has other keys")
        ref = item["ref"]
        if ref not in hints or ref in out:
            raise Malformed("an unknown or repeated ref")
        if not isinstance(item["verdicts"], list):
            raise Malformed("verdicts is not a list")
        verdicts: dict[str, tuple[str, str | None]] = {}
        named = set()
        for v in item["verdicts"]:
            if not isinstance(v, dict) or not set(v) <= {"hint", "relation", "note"}:
                raise Malformed("a verdict has other keys")
            hint, relation = v.get("hint"), v.get("relation")
            if hint not in hints[ref] or hint in named:
                raise Malformed("a hint that is not the fact's, or twice")
            named.add(hint)
            if relation == "compatible":
                continue
            if relation not in RELATIONS:
                raise Malformed("an unknown relation")
            note = v.get("note")
            if note is not None and not isinstance(note, str):
                raise Malformed("a note is not a string")
            verdicts[hint] = (relation, note)
        out[ref] = verdicts
    if set(out) != set(hints):
        raise Malformed("the answer leaves out a new fact")
    return out


# Endpoints.

class Reply:
    def __init__(self, text: str, tokens_in: int, tokens_out: int, model: str | None) -> None:
        self.text, self.tokens_in, self.tokens_out, self.model = text, tokens_in, tokens_out, model


def c4g_call(path: str, route: str, system: str, prompt: str, max_tokens: int,
             timeout: float) -> Reply:
    """One C4G turn on the qb gateway's Unix socket: chunked NDJSON events."""
    body = json.dumps({
        "route": route, "system": system,
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
        "max_tokens": max_tokens, "request_id": f"judge-eval-{time.time_ns()}",
    }).encode()
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(timeout)
        conn.connect(path)
        conn.sendall(b"POST /v1/model/turn HTTP/1.1\r\nHost: localhost\r\n"
                     b"Content-Type: application/json\r\nConnection: close\r\n"
                     + f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        raw = b""
        while chunk := conn.recv(65536):
            raw += chunk
    head, _, rest = raw.partition(b"\r\n\r\n")
    status = head.split(b" ", 2)[1] if head else b"0"
    if status != b"200":
        raise RuntimeError(f"gateway {status.decode()}: {rest[:300]!r}")
    stream = b""
    while rest:
        size_line, _, rest = rest.partition(b"\r\n")
        size = int(size_line or b"0", 16)
        if size == 0:
            break
        stream += rest[:size]
        rest = rest[size + 2:]
    text, tokens_in, tokens_out, model = "", 0, 0, None
    for event in map(json.loads, filter(None, stream.split(b"\n"))):
        kind = event["type"]
        if kind == "route":
            model = event["route"]["model"]
        elif kind == "text_delta":
            text += event["text"]
        elif kind == "usage":
            u = event["usage"]
            tokens_in = u["input"] + u.get("cache_read", 0) + u.get("cache_write", 0)
            tokens_out = u["output"]
        elif kind == "error":
            raise RuntimeError(f"gateway error event: {event.get('error')}")
    return Reply(text, tokens_in, tokens_out, model)


def openai_call(url: str, model: str, key: str, system: str, prompt: str, max_tokens: int,
                timeout: float) -> Reply:
    """One OpenAI-compatible chat completion."""
    body = json.dumps({"model": model, "max_tokens": max_tokens, "temperature": 0,
                       "messages": [{"role": "system", "content": system},
                                    {"role": "user", "content": prompt}]}).encode()
    request = urllib.request.Request(url, data=body, method="POST", headers={
        "Content-Type": "application/json", "Authorization": f"Bearer {key}"})
    with urllib.request.urlopen(request, timeout=timeout) as answer:
        data = json.load(answer)
    usage = data.get("usage") or {}
    return Reply(data["choices"][0]["message"]["content"] or "",
                 usage.get("prompt_tokens", 0), usage.get("completion_tokens", 0),
                 data.get("model", model))


# Scoring.

def score(cases: list[dict], said: dict[str, dict[str, tuple[str, str | None]] | None]) -> dict:
    """The metrics of the module docstring over the cases a call answered (`said` is None for
    a case whose call failed)."""
    judged = [c for c in cases if said.get(c["new"]["ref"]) is not None]
    contradictions = tp_contradiction = supersede_called_contradiction = 0
    other_pairs = damaged = 0
    none_cases = none_alarms = 0
    recall: dict[str, list[int]] = {r: [0, 0] for r in RELATIONS}
    confusion: Counter = Counter()
    for case in judged:
        verdicts = said[case["new"]["ref"]]
        gold = case["gold"]
        if gold["relation"] == "none":
            none_cases += 1
            none_alarms += bool(verdicts)
        for hint in case["hints"]:
            relation = verdicts.get(hint["ref"], ("compatible", None))[0]
            gold_pair = gold["relation"] != "none" and hint["ref"] == gold["ref"]
            if relation == "contradiction":
                contradictions += 1
                if gold_pair and gold["relation"] == "contradiction":
                    tp_contradiction += 1
                elif gold_pair and gold["relation"] == "supersedes":
                    supersede_called_contradiction += 1
            if gold_pair:
                confusion[(gold["relation"], relation)] += 1
            else:
                other_pairs += 1
                damaged += relation in ("duplicate", "supersedes")
        if gold["relation"] != "none" and gold["in_hints"]:
            got = verdicts.get(gold["ref"], ("compatible", None))[0]
            recall[gold["relation"]][1] += 1
            recall[gold["relation"]][0] += got == gold["relation"]
    precise = contradictions - supersede_called_contradiction
    ratio = lambda a, b: round(a / b, 4) if b else None  # noqa: E731
    return {
        "cases": len(cases),
        "judged": len(judged),
        "contradiction_precision": ratio(tp_contradiction, precise),
        "contradictions_said": contradictions,
        "silent_damage_rate": ratio(damaged, other_pairs),
        "silent_damage_pairs": [damaged, other_pairs],
        "none_false_alarm_rate": ratio(none_alarms, none_cases),
        "recall_in_hints": {r: ratio(*recall[r]) for r in RELATIONS},
        "recall_counts": recall,
        "supersedes_called_contradiction": confusion[("supersedes", "contradiction")],
        "contradiction_called_supersedes": confusion[("contradiction", "supersedes")],
        "confusion_gold_pairs": {f"{g}->{p}": n for (g, p), n in sorted(confusion.items())},
    }


def run(cases: list[dict], call, per_call: int) -> tuple[dict, list[dict], dict]:
    """Sends every batch through `call(system, prompt) -> Reply`, asking a malformed answer
    once more; the verdicts by ref, one record per call and the usage."""
    system = SYSTEM.read_text(encoding="utf-8")
    said: dict[str, dict | None] = {}
    records: list[dict] = []
    usage = {"calls": 0, "failed_calls": 0, "tokens_in": 0, "tokens_out": 0, "model": None}
    for batch in batches(cases, per_call):
        prompt = render(batch)
        verdicts, why = None, None
        for _attempt in (1, 2):
            reply = call(system, prompt)
            usage["calls"] += 1
            usage["tokens_in"] += reply.tokens_in
            usage["tokens_out"] += reply.tokens_out
            usage["model"] = reply.model or usage["model"]
            try:
                verdicts = parse(reply.text, batch)
                break
            except Malformed as e:
                why = str(e)
        if verdicts is None:
            usage["failed_calls"] += 1
        for case in batch:
            ref = case["new"]["ref"]
            said[ref] = None if verdicts is None else verdicts[ref]
            records.append({"id": case["id"], "gold": case["gold"],
                            "said": None if verdicts is None else
                            {h: {"relation": r, "note": n} for h, (r, n) in verdicts[ref].items()},
                            "failed": why if verdicts is None else None})
        print(f"judge eval: {len(said)}/{len(cases)} cases", file=sys.stderr)
    return said, records, usage


def load(limit: int | None) -> list[dict]:
    cases = [json.loads(text) for text in SET.read_text(encoding="utf-8").splitlines()]
    return cases[:limit] if limit else cases


# Self-test: no model.

SAMPLE = [
    {"id": "s1", "entity": {"id": "ada", "name": "Ada Example"},
     "new": {"ref": "f9", "text": "Ada keeps a dog named Biscuit."},
     "hints": [{"ref": "f1", "text": "Ada has a dog called Biscuit."},
               {"ref": "f2", "text": "Ада пьёт кофе."}],
     "gold": {"relation": "duplicate", "ref": "f1", "in_hints": True}},
    {"id": "s2", "entity": {"id": "bob", "name": "Bob Example"},
     "new": {"ref": "f10", "text": "Bob drinks only tea."},
     "hints": [{"ref": "f3", "text": "Bob drinks coffee every morning."}],
     "gold": {"relation": "contradiction", "ref": "f3", "in_hints": True}},
    {"id": "s3", "entity": {"id": "ada", "name": "Ada Example"},
     "new": {"ref": "f11", "text": "Ada plays chess."},
     "hints": [{"ref": "f2", "text": "Ада пьёт кофе."}],
     "gold": {"relation": "none", "ref": "", "in_hints": False}},
]


def self_test() -> int:
    golden = GOLDEN.read_text(encoding="utf-8")
    if render(SAMPLE) != golden:
        print("judge eval: the rendering differs from the engine's golden file", file=sys.stderr)
        return 1

    def gold_judge(_system: str, prompt: str) -> Reply:
        items = []
        for text in prompt.splitlines():
            row = json.loads(text)
            if "new" in row:
                case = next(c for c in SAMPLE if c["new"]["ref"] == row["new"]["ref"])
                g = case["gold"]
                verdicts = [] if g["relation"] == "none" else [
                    {"hint": g["ref"], "relation": g["relation"], "note": "gold"}]
                items.append({"ref": row["new"]["ref"], "verdicts": verdicts})
        return Reply(json.dumps({"items": items}), 100, 10, "scripted")

    said, _, usage = run(SAMPLE, gold_judge, 25)
    perfect = score(SAMPLE, said)
    if (perfect["contradiction_precision"], perfect["silent_damage_rate"],
            usage["calls"]) != (1.0, 0.0, 1):
        print(f"judge eval: a gold judge scored {perfect}", file=sys.stderr)
        return 1

    def careless(_system: str, prompt: str) -> Reply:
        items = [{"ref": json.loads(t)["new"]["ref"],
                  "verdicts": [{"hint": h, "relation": "duplicate"}
                               for h in json.loads(t)["new"]["hints"]]}
                 for t in prompt.splitlines() if "new" in json.loads(t)]
        return Reply(json.dumps({"items": items}), 100, 10, "scripted")

    said, _, _ = run(SAMPLE, careless, 25)
    bad = score(SAMPLE, said)
    if bad["silent_damage_rate"] != 1.0 or bad["recall_in_hints"]["contradiction"] != 0.0:
        print(f"judge eval: a careless judge scored {bad}", file=sys.stderr)
        return 1

    def broken(_system: str, _prompt: str) -> Reply:
        return Reply("no json", 1, 1, "scripted")

    said, _, usage = run(SAMPLE, broken, 25)
    if usage["calls"] != 2 or usage["failed_calls"] != 1 or score(SAMPLE, said)["judged"] != 0:
        print(f"judge eval: a malformed judge: {usage}", file=sys.stderr)
        return 1
    print("judge eval: self-test passed")
    return 0


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--gateway", help="the qb gateway's C4G socket")
    p.add_argument("--route", default="small", help="the C4G route alias (default small)")
    p.add_argument("--openai-url", help="an OpenAI-compatible chat completions URL")
    p.add_argument("--model", help="the model name for --openai-url")
    p.add_argument("--batch", type=int, default=25, help="new facts per call (default 25)")
    p.add_argument("--max-tokens", type=int, default=8192)
    p.add_argument("--timeout", type=float, default=600)
    p.add_argument("--limit", type=int, help="only the first N cases")
    p.add_argument("--out", type=Path, help="write the scores and every case's verdicts here")
    args = p.parse_args(argv)
    if args.self_test:
        return self_test()
    if bool(args.gateway) == bool(args.openai_url):
        p.error("give exactly one of --gateway and --openai-url")
    if args.gateway:
        def call(system: str, prompt: str) -> Reply:
            return c4g_call(args.gateway, args.route, system, prompt, args.max_tokens,
                            args.timeout)
        judge = args.route
    else:
        key = os.environ.get("JUDGE_EVAL_API_KEY")
        if not key or not args.model:
            p.error("--openai-url needs --model and JUDGE_EVAL_API_KEY in the environment")

        def call(system: str, prompt: str) -> Reply:
            return openai_call(args.openai_url, args.model, key, system, prompt, args.max_tokens,
                               args.timeout)
        judge = args.model
    cases = load(args.limit)
    said, records, usage = run(cases, call, args.batch)
    result = {"judge": f"{usage['model'] or judge} {PROMPT_VERSION}", "usage": usage,
              "scores": score(cases, said), "hint_recall_e5_small": 0.886}
    print(json.dumps(result, indent=2, ensure_ascii=False))
    if args.out:
        args.out.write_text(json.dumps({**result, "cases": records}, ensure_ascii=False, indent=1),
                            encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
