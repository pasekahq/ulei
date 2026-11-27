"""Scores a judge of the night's stage d (ADR-0096) on the synthetic judge eval set.

The set, ``ci/reconcile_eval/judge-eval.jsonl``, holds the 419 new facts of the frozen selection
set (``ci/reconcile_eval/gen.py``), each with the earlier facts e5-small picked as its hints in
report E15 and its gold relation to one earlier fact: duplicate, supersedes, contradiction or
none, true by construction. The runner sends the cases as the night does: the system prompt
``crates/engine/src/night/judge.txt``, whole entities per call (``--batch``, default 25), the
same JSON lines and the same reading of the answer as ``night::judge::parse``: the JSON object
alone, in a code fence or within prose, read per new fact, so a missing or defective item leaves
only its own fact undecided (a *dropped* case, unscored, as the night leaves it waiting); an
answer with no object, or deciding no fact, is malformed and asked once more, then the call
fails and its cases are unscored.

It calls a model, so it is never a required check: the workflow ``judge-eval.yml`` runs it with
``--claude`` on demand, weekly and on pull requests that touch the judge, when a Claude
credential is set (``docs/runbooks/night-gardener.md`` section 6). Give it one endpoint:

- ``--gateway SOCKET --route ALIAS``: the qb gateway (C4G) the night uses; qb holds the keys.
- ``--openai-url URL --model NAME``: any OpenAI-compatible chat completions endpoint, the key
  in the environment variable ``JUDGE_EVAL_API_KEY``.
- ``--claude [--model ALIAS] [--claude-bin PATH]``: the Claude Code CLI in headless mode, on
  the account the CLI is logged in to (a subscription needs no key). Each call is a fresh,
  context-free session: the CLI runs in an empty temporary directory (no CLAUDE.md, no
  project memory), with the judge's prompt as the whole system prompt, no tools, no skills,
  no MCP servers, no settings files and no saved session; the prompt goes in on stdin and the
  answer is read from the JSON envelope. ``--timeout`` bounds each call; a timeout, a failed
  exit or an unreadable envelope counts as a malformed answer (asked once more, then a failed
  call); an error envelope that is an authentication or usage-limit error stops the run at
  once with exit 2 and that error, in ``error`` of ``--out``. The cost the envelope reports is
  summed into ``usage.cost_usd``. Example:
  ``python3 ci/judge_eval.py --claude --model sonnet --out judge-eval.json``; add
  ``--limit 6`` for a cheap smoke run.

and it prints, and writes as JSON to ``--out``:

- ``scored`` of ``cases``: the cases decided by a call; failed calls (``cases_failed``) and
  dropped cases (``cases_dropped``) are counted apart and every rate below is over the scored
  cases only, so compare runs on ``scored`` too;
- case accuracy, overall and per gold relation, on all scored cases and on those in reach (gold
  fact among the hints, or ``none``): a case is right when its gold pair got the gold relation
  and every other hint none (``none``: no verdict at all); a case whose gold fact is outside the
  hints is never right;
- precision per relation over all pairs said (contradiction precision leaves out a supersedes
  case called a contradiction, counted apart as confusion, not as an error);
- silent-damage rate (ADR-0096): of the pairs known compatible, the share called ``duplicate``
  or ``supersedes``, which would retract a fact silently. Known compatible is every pair but the
  gold pair and the pairs a case lists under ``gold.also`` (hints related to the new fact too,
  with the relation that is right for them, e.g. a supersede of "drives to work" by "no longer
  owns a car" when the car fact itself is outside the hints);
- recall per relation on the cases whose gold fact is among the hints (the judge's own recall;
  times the hint recall, 0.886 for e5-small, for end to end);
- the confusion on gold pairs, the calls, failed calls and tokens.

``--save-raw DIR`` keeps the raw replies of every call with a malformed attempt or a dropped
case, one JSON file per call. ``--gate`` exits 1 unless the run meets the provisional thresholds
of ``GATE`` (``docs/runbooks/night-gardener.md`` section 6). ``--rescore FILE`` scores (and
gates) the per-case verdicts of an earlier ``--out`` file again under the current set and
metrics, without a model. ``--summary-md FILE`` appends a compact Markdown table of the run (the
scores, each gate threshold, cost and wall time) to FILE, e.g. ``$GITHUB_STEP_SUMMARY``.

``python3 ci/judge_eval.py --self-test`` checks the rendering against the engine's golden file,
the reading of answers and the scoring on scripted judges, without any model. Synthetic data
only.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SET = ROOT / "ci" / "reconcile_eval" / "judge-eval.jsonl"
SYSTEM = ROOT / "crates" / "engine" / "src" / "night" / "judge.txt"
GOLDEN = ROOT / "crates" / "engine" / "tests" / "fixtures" / "judge-prompt.txt"
PROMPT_VERSION = "judge-v2"
RELATIONS = ("duplicate", "supersedes", "contradiction")
# Provisional thresholds of `--gate`, set from the first three-model run (report E19):
# (metric, at least or at most, bound).
GATE = (
    ("silent_damage_rate", "max", 0.005),
    ("precision.duplicate", "min", 0.98),
    ("contradiction_precision", "min", 0.80),
    ("recall_in_hints.supersedes", "min", 0.75),
    ("recall_in_hints.contradiction", "min", 0.60),
    ("unscored_rate", "max", 0.05),
)
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


def answer_object(text: str) -> dict:
    """The JSON object of an answer as `night::judge` finds it: the whole text, or a code fence
    that starts it, else the span from the first `{` to the last `}`."""
    def whole(text: str) -> dict:
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
        if not isinstance(root, dict):
            raise Malformed("answer is not a JSON object")
        return root
    try:
        return whole(text)
    except Malformed:
        start, end = text.find("{"), text.rfind("}")
        if start < 0 or end <= start:
            raise
        try:
            return whole(text[start:end + 1])
        except Malformed:
            pass
        raise


def verdicts_of(item: dict, hints: set[str]) -> dict[str, tuple[str, str | None]] | str:
    """The verdicts of one item as `night::judge::verdicts_of` reads them, or why it is
    defective."""
    if set(item) != {"ref", "verdicts"}:
        return "an item has other keys or no verdicts"
    if not isinstance(item["verdicts"], list):
        return "verdicts is not a list"
    verdicts: dict[str, tuple[str, str | None]] = {}
    named = set()
    for v in item["verdicts"]:
        if not isinstance(v, dict) or not set(v) <= {"hint", "relation", "note"}:
            return "a verdict has other keys"
        hint, relation = v.get("hint"), v.get("relation")
        if not isinstance(hint, str) or hint not in hints or hint in named:
            return "a hint that is not the fact's, or twice"
        named.add(hint)
        if relation == "compatible":
            continue
        if relation not in RELATIONS:
            return "an unknown relation"
        note = v.get("note")
        if note is not None and not isinstance(note, str):
            return "a note is not a string"
        verdicts[hint] = (relation, note.strip() or None if note else None)
    return verdicts


def parse(text: str, batch: list[dict]) -> tuple[dict[str, dict[str, tuple[str, str | None]]],
                                                 dict[str, str]]:
    """The verdicts of an answer by new ref and hint, and why each undecided new fact was
    dropped, read as `night::judge::parse` reads them: per new fact, a fact named by a defective
    item, named twice or not named is dropped with none of its verdicts; an item naming no new
    fact of the batch is ignored. Malformed without an object of items or without a decided
    fact."""
    root = answer_object(text)
    if set(root) != {"items"} or not isinstance(root["items"], list):
        raise Malformed("answer is not an object of items")
    hints = {c["new"]["ref"]: {h["ref"] for h in c["hints"]} for c in batch}
    out: dict[str, dict[str, tuple[str, str | None]]] = {}
    dropped: dict[str, str] = {}
    for item in root["items"]:
        if not isinstance(item, dict) or not isinstance(item.get("ref"), str):
            continue
        ref = item["ref"]
        if ref not in hints:
            continue
        if ref in out or ref in dropped:
            out.pop(ref, None)
            dropped[ref] = "the fact is named twice"
            continue
        verdicts = verdicts_of(item, hints[ref])
        if isinstance(verdicts, str):
            dropped[ref] = verdicts
        else:
            out[ref] = verdicts
    for ref in hints:
        if ref not in out:
            dropped.setdefault(ref, "the answer leaves the fact out")
    if not out:
        raise Malformed("the answer decides no new fact")
    return out, dropped


# Endpoints.

class Reply:
    def __init__(self, text: str, tokens_in: int, tokens_out: int, model: str | None,
                 cost_usd: float = 0.0) -> None:
        self.text, self.tokens_in, self.tokens_out, self.model = text, tokens_in, tokens_out, model
        self.cost_usd = cost_usd


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


class Refused(Exception):
    """The endpoint refuses every call: no or a wrong credential, or a usage limit. The run
    stops at once instead of counting each call as malformed."""


# What the CLI's error envelope says when the API, or the CLI itself, refuses the account.
REFUSAL_TYPES = ("authentication_error", "permission_error", "billing_error",
                 "rate_limit_error")
REFUSAL_TEXTS = ("not logged in", "/login", "invalid api key", "failed to authenticate",
                 "usage limit", "limit reached", "hit your limit", "credit balance")


def cli_refusal(envelope: dict) -> str | None:
    """The reason of an error envelope of the CLI that is an authentication or usage-limit
    error, else None."""
    if not envelope.get("is_error"):
        return None
    said = str(envelope.get("result") or "")
    status, kind = envelope.get("api_error_status"), envelope.get("api_error_type")
    if (status in (401, 402, 403, 429) or kind in REFUSAL_TYPES
            or any(t in said.lower() for t in REFUSAL_TEXTS)):
        detail = ", ".join(str(x) for x in (status, kind) if x)
        return f"{said[:300]}" + (f" ({detail})" if detail else "")
    return None


def claude_call(binary: str, model: str | None, system: str, prompt: str,
                timeout: float) -> Reply:
    """One headless Claude Code turn: an empty working directory, the judge's prompt as the
    whole system prompt, no tools, skills, MCP servers, settings or saved session, the prompt
    on stdin and the answer in the JSON envelope. A call that fails in any way returns an
    empty answer, which the caller counts as malformed."""
    argv = [binary, "-p", "--output-format", "json", "--system-prompt", system, "--tools", "",
            "--disable-slash-commands", "--strict-mcp-config", "--setting-sources", "",
            "--no-session-persistence"]
    if model:
        argv += ["--model", model]
    try:
        with tempfile.TemporaryDirectory(prefix="judge-eval-") as empty:
            done = subprocess.run(argv, input=prompt, capture_output=True, text=True,
                                  encoding="utf-8", cwd=empty, timeout=timeout)
        try:
            envelope = json.loads(done.stdout)
        except ValueError:
            envelope = None
        # The CLI exits 1 with an error envelope on stdout when the API refuses the call.
        refusal = cli_refusal(envelope) if isinstance(envelope, dict) else None
        if refusal:
            raise Refused(refusal)
        if done.returncode != 0:
            said = envelope.get("result") if isinstance(envelope, dict) else None
            raise RuntimeError(f"exit {done.returncode}: "
                               f"{str(said or done.stderr.strip())[:200]}")
        if not isinstance(envelope, dict):
            raise ValueError(f"no JSON envelope: {done.stdout[:200]!r}")
        if envelope.get("is_error") or not isinstance(envelope.get("result"), str):
            raise RuntimeError(f"error envelope: {str(envelope.get('result'))[:200]}")
    except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as e:
        print(f"judge eval: claude call failed: {type(e).__name__}: {e}", file=sys.stderr)
        return Reply("", 0, 0, None)
    u = envelope.get("usage") or {}
    models = envelope.get("modelUsage") or {}
    return Reply(envelope["result"],
                 u.get("input_tokens", 0) + u.get("cache_read_input_tokens", 0)
                 + u.get("cache_creation_input_tokens", 0),
                 u.get("output_tokens", 0), next(iter(models), None),
                 float(envelope.get("total_cost_usd") or 0.0))


# Scoring.

def expected(case: dict) -> dict[str, str]:
    """The relation that is right for each hint of `case` other than compatible: the gold pair
    and the pairs of `gold.also`."""
    gold = case["gold"]
    out = {a["ref"]: a["relation"] for a in gold.get("also", [])}
    if gold["relation"] != "none" and gold["in_hints"]:
        out[gold["ref"]] = gold["relation"]
    return out


def score(cases: list[dict], said: dict[str, dict[str, tuple[str, str | None]] | None],
          unscored: dict[str, int] | None = None) -> dict:
    """The metrics of the module docstring over the cases a call decided (`said` is None or
    absent for a case whose call failed or that was dropped); `unscored` counts both."""
    unscored = unscored or {}
    judged = [c for c in cases if said.get(c["new"]["ref"]) is not None]
    contradictions = tp_contradiction = supersede_called_contradiction = 0
    compatible_pairs = damaged = 0
    none_cases = none_alarms = 0
    recall: dict[str, list[int]] = {r: [0, 0] for r in RELATIONS}
    precision: dict[str, list[int]] = {r: [0, 0] for r in RELATIONS}
    golds = RELATIONS + ("none",)
    accuracy = {r: [0, 0] for r in golds}
    in_reach = {r: [0, 0] for r in golds}
    confusion: Counter = Counter()
    for case in judged:
        verdicts = said[case["new"]["ref"]]
        gold = case["gold"]
        right_pairs = expected(case)
        if gold["relation"] == "none":
            none_cases += 1
            none_alarms += bool(verdicts)
        right = gold["relation"] == "none" or bool(gold["in_hints"])
        for hint in case["hints"]:
            relation = verdicts.get(hint["ref"], ("compatible", None))[0]
            want = right_pairs.get(hint["ref"], "compatible")
            right = right and relation == want
            gold_pair = gold["relation"] != "none" and hint["ref"] == gold["ref"]
            if relation in RELATIONS:
                precision[relation][1] += 1
                precision[relation][0] += relation == want
            if relation == "contradiction":
                contradictions += 1
                if gold_pair and gold["relation"] == "contradiction":
                    tp_contradiction += 1
                elif gold_pair and gold["relation"] == "supersedes":
                    supersede_called_contradiction += 1
            if gold_pair:
                confusion[(gold["relation"], relation)] += 1
            elif hint["ref"] not in right_pairs:
                compatible_pairs += 1
                damaged += relation in ("duplicate", "supersedes")
        accuracy[gold["relation"]][1] += 1
        accuracy[gold["relation"]][0] += right
        if gold["relation"] == "none" or gold["in_hints"]:
            in_reach[gold["relation"]][1] += 1
            in_reach[gold["relation"]][0] += right
        if gold["relation"] != "none" and gold["in_hints"]:
            got = verdicts.get(gold["ref"], ("compatible", None))[0]
            recall[gold["relation"]][1] += 1
            recall[gold["relation"]][0] += got == gold["relation"]
    precise = contradictions - supersede_called_contradiction
    ratio = lambda a, b: round(a / b, 4) if b else None  # noqa: E731

    def rates(counts: dict[str, list[int]]) -> dict:
        right, total = sum(c[0] for c in counts.values()), sum(c[1] for c in counts.values())
        return {"all": ratio(right, total), "scored": total,
                **{r: ratio(*counts[r]) for r in counts}, "counts": counts}
    failed, dropped = unscored.get("failed", 0), unscored.get("dropped", 0)
    return {
        "cases": len(cases),
        "scored": len(judged),
        "judged": len(judged),
        "cases_failed": failed,
        "cases_dropped": dropped,
        "unscored_rate": ratio(failed + dropped, len(cases)),
        "accuracy": rates(accuracy),
        "accuracy_in_reach": rates(in_reach),
        "precision": {r: ratio(*precision[r]) for r in RELATIONS},
        "precision_counts": precision,
        "contradiction_precision": ratio(tp_contradiction, precise),
        "contradictions_said": contradictions,
        "silent_damage_rate": ratio(damaged, compatible_pairs),
        "silent_damage_pairs": [damaged, compatible_pairs],
        "none_false_alarm_rate": ratio(none_alarms, none_cases),
        "recall_in_hints": {r: ratio(*recall[r]) for r in RELATIONS},
        "recall_counts": recall,
        "supersedes_called_contradiction": confusion[("supersedes", "contradiction")],
        "contradiction_called_supersedes": confusion[("contradiction", "supersedes")],
        "confusion_gold_pairs": {f"{g}->{p}": n for (g, p), n in sorted(confusion.items())},
    }


def gate_rows(scores: dict) -> list[tuple[str, str, float, float | None, bool]]:
    """Each threshold of `GATE` with the value of `scores` and whether it is met; a metric
    with no value misses."""
    rows = []
    for metric, kind, bound in GATE:
        value = scores
        for key in metric.split("."):
            value = value.get(key) if isinstance(value, dict) else None
        ok = value is not None and (value <= bound if kind == "max" else value >= bound)
        rows.append((metric, kind, bound, value, ok))
    return rows


def gate(scores: dict) -> list[str]:
    """The thresholds of `GATE` that `scores` misses; a metric with no value misses."""
    return [f"{metric} {value} ({'at most' if kind == 'max' else 'at least'} {bound})"
            for metric, kind, bound, value, ok in gate_rows(scores) if not ok]


def summary_md(result: dict) -> str:
    """A compact Markdown table of a run, for a CI step summary."""
    def num(value, digits: int = 3) -> str:
        return "n/a" if value is None else f"{value:.{digits}f}"
    usage = result.get("usage") or {}
    out = [f"### Judge eval: {result.get('judge') or 'unknown judge'}", ""]
    if result.get("error"):
        out += [f"**Stopped:** {result['error']}", ""]
    s = result.get("scores")
    if s:
        damage = s.get("silent_damage_pairs") or [0, 0]
        out += ["| | |", "|---|---|",
                f"| cases scored / total | {s['scored']} / {s['cases']} |",
                f"| calls / failed calls | {usage.get('calls', 'n/a')} / "
                f"{usage.get('failed_calls', 'n/a')} ({s['cases_failed']} cases) |",
                f"| dropped facts | {s['cases_dropped']} |",
                f"| unscored rate | {num(s['unscored_rate'])} |",
                f"| case accuracy (in reach) | {num(s['accuracy']['all'])} "
                f"({num(s['accuracy_in_reach']['all'])}) |",
                f"| silent damage | {num(s['silent_damage_rate'], 4)} "
                f"({damage[0]}/{damage[1]} pairs) |",
                f"| cost (reported by the endpoint) | {num(usage.get('cost_usd'))} USD |",
                f"| wall time | {num(usage.get('wall_seconds'), 0)} s |", "",
                "| relation | precision | recall in hints |", "|---|---|---|"]
        for r in RELATIONS:
            precision = s["contradiction_precision"] if r == "contradiction" else s["precision"][r]
            out.append(f"| {r} | {num(precision)} | {num(s['recall_in_hints'][r])} |")
        out += ["", "| gate (provisional) | value | bound | |", "|---|---|---|---|"]
        for metric, kind, bound, value, ok in gate_rows(s):
            out.append(f"| {metric} | {num(value, 4)} | {'≤' if kind == 'max' else '≥'} "
                       f"{bound} | {'pass' if ok else '**FAIL**'} |")
    return "\n".join(out) + "\n"


def run(cases: list[dict], call, per_call: int,
        save_raw: Path | None = None) -> tuple[dict, list[dict], dict]:
    """Sends every batch through `call(system, prompt) -> Reply`, asking a malformed answer
    once more; the verdicts by ref (None for an unscored case), one record per case and the
    usage. With `save_raw`, the raw replies of a call with a malformed attempt or a dropped
    case go to `save_raw/call-NNN.json`."""
    system = SYSTEM.read_text(encoding="utf-8")
    said: dict[str, dict | None] = {}
    records: list[dict] = []
    usage = {"calls": 0, "failed_calls": 0, "cases_failed": 0, "cases_dropped": 0,
             "calls_with_drops": 0, "tokens_in": 0, "tokens_out": 0, "cost_usd": 0.0,
             "model": None}
    for number, batch in enumerate(batches(cases, per_call)):
        prompt = render(batch)
        verdicts, dropped, why, attempts = None, {}, None, []
        for _attempt in (1, 2):
            reply = call(system, prompt)
            usage["calls"] += 1
            usage["tokens_in"] += reply.tokens_in
            usage["tokens_out"] += reply.tokens_out
            usage["cost_usd"] = round(usage["cost_usd"] + reply.cost_usd, 6)
            usage["model"] = reply.model or usage["model"]
            try:
                verdicts, dropped = parse(reply.text, batch)
                attempts.append({"text": reply.text, "dropped": dropped})
                break
            except Malformed as e:
                why = str(e)
                attempts.append({"text": reply.text, "malformed": why})
        if verdicts is None:
            usage["failed_calls"] += 1
            usage["cases_failed"] += len(batch)
        elif dropped:
            usage["calls_with_drops"] += 1
            usage["cases_dropped"] += len(dropped)
        if save_raw and (verdicts is None or dropped or len(attempts) > 1):
            save_raw.mkdir(parents=True, exist_ok=True)
            (save_raw / f"call-{number:03d}.json").write_text(json.dumps(
                {"call": number, "cases": [c["id"] for c in batch], "attempts": attempts},
                ensure_ascii=False, indent=1), encoding="utf-8")
        for case in batch:
            ref = case["new"]["ref"]
            mine = None if verdicts is None else verdicts.get(ref)
            said[ref] = mine
            records.append({"id": case["id"], "gold": case["gold"],
                            "said": None if mine is None else
                            {h: {"relation": r, "note": n} for h, (r, n) in mine.items()},
                            "failed": why if verdicts is None else None,
                            "dropped": dropped.get(ref)})
        print(f"judge eval: {len(said)}/{len(cases)} cases", file=sys.stderr)
    return said, records, usage


def rescore(path: Path, cases: list[dict]) -> tuple[dict, dict]:
    """The verdicts and unscored counts of an earlier `--out` file, by the current set."""
    earlier = json.loads(path.read_text(encoding="utf-8"))
    ids = {c["id"]: c["new"]["ref"] for c in cases}
    said: dict[str, dict | None] = {}
    unscored = {"failed": 0, "dropped": 0}
    for record in earlier["cases"]:
        if record["id"] not in ids:
            continue
        if record["said"] is None:
            unscored["dropped" if record.get("dropped") else "failed"] += 1
            said[ids[record["id"]]] = None
        else:
            said[ids[record["id"]]] = {h: (v["relation"], v.get("note"))
                                       for h, v in record["said"].items()}
    return said, unscored


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
    errors: list[str] = []

    def check(ok: bool, what: str) -> None:
        if not ok:
            errors.append(what)

    golden = GOLDEN.read_text(encoding="utf-8")
    check(render(SAMPLE) == golden, "the rendering differs from the engine's golden file")

    def scripted(items_of):
        def call(_system: str, prompt: str) -> Reply:
            rows = [json.loads(t)["new"] for t in prompt.splitlines() if "new" in json.loads(t)]
            return Reply(items_of(rows), 100, 10, "scripted")
        return call

    def gold_items(rows: list[dict]) -> str:
        items = []
        for row in rows:
            g = next(c for c in SAMPLE if c["new"]["ref"] == row["ref"])["gold"]
            verdicts = [] if g["relation"] == "none" else [
                {"hint": g["ref"], "relation": g["relation"], "note": "gold"}]
            items.append({"ref": row["ref"], "verdicts": verdicts})
        return json.dumps({"items": items})

    said, _, usage = run(SAMPLE, scripted(gold_items), 25)
    perfect = score(SAMPLE, said)
    check((perfect["contradiction_precision"], perfect["silent_damage_rate"], usage["calls"],
           perfect["accuracy"]["all"], perfect["scored"])
          == (1.0, 0.0, 1, 1.0, 3), f"a gold judge scored {perfect}")
    check([m.split()[0] for m in gate(perfect)] == ["recall_in_hints.supersedes"],
          "the gate misses a metric with no value (the sample has no supersedes case)")
    passing = {**perfect, "recall_in_hints": {**perfect["recall_in_hints"], "supersedes": 0.75}}
    check(gate(passing) == [], f"the gate on {passing}")

    careless = scripted(lambda rows: json.dumps({"items": [
        {"ref": r["ref"], "verdicts": [{"hint": h, "relation": "duplicate"} for h in r["hints"]]}
        for r in rows]}))
    said, _, _ = run(SAMPLE, careless, 25)
    bad = score(SAMPLE, said)
    check(bad["silent_damage_rate"] == 1.0 and bad["recall_in_hints"]["contradiction"] == 0.0
          and bad["accuracy"]["all"] == 0.0 and gate(bad), f"a careless judge scored {bad}")

    said, _, usage = run(SAMPLE, scripted(lambda _rows: "no json"), 25)
    check(usage["calls"] == 2 and usage["failed_calls"] == 1 and usage["cases_failed"] == 3
          and score(SAMPLE, said)["scored"] == 0, f"a malformed judge: {usage}")

    # The reading of answers, as night::judge::parse: per fact, in a fence or prose.
    batch = SAMPLE
    ok = {"ref": "f9", "verdicts": [{"hint": "f1", "relation": "duplicate"}]}
    body = json.dumps({"items": [ok, {"ref": "f10", "verdicts": []},
                                 {"ref": "f11", "verdicts": []}]})
    for text in (body, f"```json\n{body}\n```", f"Here:\n```json\n{body}\n```\nDone.",
                 f"Sure. {body} Bye."):
        try:
            verdicts, dropped = parse(text, batch)
            check(set(verdicts) == {"f9", "f10", "f11"} and not dropped, f"read {text!r}")
        except Malformed as e:
            check(False, f"{text!r} refused: {e}")
    for bad_item, why in (
            ({"ref": "f10", "verdicts": [{"hint": "f1", "relation": "duplicate"}]},
             "a hint of another fact of the call"),
            ({"ref": "f10", "verdicts": [{"hint": "f3", "relation": "same"}]}, "unknown relation"),
            ({"ref": "f10", "verdicts": [{"hint": "f3"}]}, "no relation"),
            ({"ref": "f10", "verdicts": [{"hint": "f3", "relation": "duplicate"},
                                         {"hint": "f3", "relation": "supersedes"}]}, "hint twice"),
            ({"ref": "f10", "verdicts": [{"hint": "f3", "relation": "contradiction",
                                          "note": 7}]}, "note not text"),
            ({"ref": "f10", "verdicts": [], "why": "x"}, "item with another key"),
            ({"ref": "f10", "verdicts": "none"}, "verdicts not a list")):
        verdicts, dropped = parse(json.dumps({"items": [ok, bad_item,
                                                        {"ref": "f11", "verdicts": []}]}), batch)
        check(set(verdicts) == {"f9", "f11"} and set(dropped) == {"f10"}, why)
    rest = [{"ref": "f10", "verdicts": []}, {"ref": "f11", "verdicts": []}]
    verdicts, dropped = parse(json.dumps({"items": [{"ref": "f7", "verdicts": []}, "f10",
                                                    {"ref": 10}, ok, *rest]}), batch)
    check(set(verdicts) == {"f9", "f10", "f11"} and not dropped, "an unknown ref is ignored")
    verdicts, dropped = parse(json.dumps({"items": [ok, ok, *rest]}), batch)
    check(set(verdicts) == {"f10", "f11"} and set(dropped) == {"f9"}, "a fact named twice")
    verdicts, dropped = parse(json.dumps({"items": rest[:1]}), batch)
    check(set(verdicts) == {"f10"} and set(dropped) == {"f9", "f11"}, "facts left out")
    for text in ("no json", "I cannot judge these.", '{"items": [', "[]", '{"items": []}',
                 '{"items": [], "extra": 1}', json.dumps({"items": [
                     {"ref": "f9", "verdicts": [{"hint": "f9", "relation": "duplicate"}]}]})):
        try:
            parse(text, batch)
            check(False, f"{text!r} was read")
        except Malformed:
            pass

    # A stray verdict drops its case from the score and the call is not asked again.
    def stray(rows: list[dict]) -> str:
        items = json.loads(gold_items(rows))["items"]
        for item in items:
            if item["ref"] == "f11":
                item["verdicts"] = [{"hint": "f1", "relation": "supersedes"}]
        return json.dumps({"items": items})
    with tempfile.TemporaryDirectory(prefix="judge-eval-test-") as raw:
        said, records, usage = run(SAMPLE, scripted(stray), 25, Path(raw))
        saved = sorted(Path(raw).iterdir())
        check(len(saved) == 1 and "supersedes" in saved[0].read_text(encoding="utf-8"),
              "the raw reply of a call with a dropped case is saved")
    scores = score(SAMPLE, said, {"dropped": usage["cases_dropped"]})
    check((usage["calls"], usage["failed_calls"], usage["cases_dropped"], scores["scored"],
           scores["unscored_rate"], scores["silent_damage_pairs"][0])
          == (1, 0, 1, 2, round(1 / 3, 4), 0), f"a stray verdict: {usage} {scores}")
    check({r["id"]: r["dropped"] for r in records}
          == {"s1": None, "s2": None, "s3": "a hint that is not the fact's, or twice"},
          f"records of a stray verdict: {records}")
    check(any(m.startswith("unscored_rate") for m in gate(scores)),
          "the gate counts dropped cases")

    # A related hint listed under gold.also is no damage and right; any other one is damage.
    case = {"id": "s4", "entity": {"id": "eve", "name": "Eve Example"},
            "new": {"ref": "f20", "text": "Since spring Eve has no car."},
            "hints": [{"ref": "f21", "text": "Eve drives to work."},
                      {"ref": "f22", "text": "Eve drinks tea."}],
            "gold": {"relation": "supersedes", "ref": "f23", "in_hints": False,
                     "also": [{"ref": "f21", "relation": "supersedes"}]}}
    related = score([case], {"f20": {"f21": ("supersedes", None)}})
    check(related["silent_damage_pairs"] == [0, 1] and related["precision"]["supersedes"] == 1.0,
          f"an also pair: {related}")
    damage = score([case], {"f20": {"f22": ("duplicate", None)}})
    check(damage["silent_damage_pairs"] == [1, 1], f"a compatible pair: {damage}")
    c288 = next(c for c in load(None) if c["id"] == "c288")
    check(c288["gold"].get("also") == [{"ref": "e41.f23", "relation": "supersedes"}],
          "case c288 lists its related hint")

    # The CLI's refusals (the envelopes it printed with no, a wrong and a bad OAuth credential)
    # stop the run; another error envelope is a malformed answer.
    for envelope in (
            {"is_error": True, "api_error_status": None,
             "result": "Not logged in · Please run /login"},
            {"is_error": True, "api_error_status": 401, "api_error_type": "authentication_error",
             "result": "Invalid API key · Fix external API key"},
            {"is_error": True, "api_error_status": 429, "result": "Claude AI usage limit reached"}):
        check(cli_refusal(envelope) is not None, f"a refusal not seen: {envelope}")
    check(cli_refusal({"is_error": True, "api_error_status": 500, "result": "Overloaded"}) is None
          and cli_refusal({"is_error": False, "result": "{}"}) is None, "a refusal too many")
    with tempfile.TemporaryDirectory(prefix="judge-eval-test-") as tmp:
        fake = Path(tmp) / "claude"
        fake.write_text("#!/bin/sh\ncat >/dev/null\necho '{\"is_error\": true, \"api_error_status\""
                        ": null, \"result\": \"Not logged in\"}'\nexit 1\n", encoding="utf-8")
        fake.chmod(0o755)
        try:
            claude_call(str(fake), None, "system", "prompt", 30)
            check(False, "a refused CLI call did not stop the run")
        except Refused as e:
            check("Not logged in" in str(e), f"the refusal's reason: {e}")
    table = summary_md({"judge": "scripted judge-v2", "usage": {"calls": 1, "failed_calls": 0},
                        "scores": perfect})
    check("| recall_in_hints.supersedes | n/a | ≥ 0.75 | **FAIL** |" in table
          and "| duplicate | 1.000 | 1.000 |" in table, f"the summary table:\n{table}")

    for what in errors:
        print(f"judge eval: {what}", file=sys.stderr)
    if errors:
        return 1
    print("judge eval: self-test passed")
    return 0


def main(argv: list[str]) -> int:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--gateway", help="the qb gateway's C4G socket")
    p.add_argument("--route", default="small", help="the C4G route alias (default small)")
    p.add_argument("--openai-url", help="an OpenAI-compatible chat completions URL")
    p.add_argument("--model", help="the model name for --openai-url, the alias for --claude")
    p.add_argument("--claude", action="store_true", help="the Claude Code CLI, headless")
    p.add_argument("--claude-bin", default="claude", help="the CLI binary (default claude)")
    p.add_argument("--batch", type=int, default=25, help="new facts per call (default 25)")
    p.add_argument("--max-tokens", type=int, default=8192)
    p.add_argument("--timeout", type=float, default=600)
    p.add_argument("--limit", type=int, help="only the first N cases")
    p.add_argument("--out", type=Path, help="write the scores and every case's verdicts here")
    p.add_argument("--save-raw", type=Path, metavar="DIR",
                   help="keep the raw replies of calls with a malformed attempt or a dropped case")
    p.add_argument("--gate", action="store_true",
                   help="exit 1 unless the run meets the provisional thresholds")
    p.add_argument("--rescore", type=Path, metavar="FILE",
                   help="score the verdicts of an earlier --out file again, no model")
    p.add_argument("--summary-md", type=Path, metavar="FILE",
                   help="append a Markdown table of the run (e.g. $GITHUB_STEP_SUMMARY)")
    args = p.parse_args(argv)
    if args.self_test:
        return self_test()
    cases = load(args.limit)
    if args.rescore:
        said, unscored = rescore(args.rescore, cases)
        earlier = json.loads(args.rescore.read_text(encoding="utf-8"))
        result = {"judge": earlier.get("judge"), "rescored": str(args.rescore),
                  "scores": score(cases, said, unscored)}
        records = None
    else:
        if bool(args.gateway) + bool(args.openai_url) + args.claude != 1:
            p.error("give exactly one of --gateway, --openai-url, --claude and --rescore")
        if args.claude:
            def call(system: str, prompt: str) -> Reply:
                return claude_call(args.claude_bin, args.model, system, prompt, args.timeout)
            judge = args.model or "claude"
        elif args.gateway:
            def call(system: str, prompt: str) -> Reply:
                return c4g_call(args.gateway, args.route, system, prompt, args.max_tokens,
                                args.timeout)
            judge = args.route
        else:
            key = os.environ.get("JUDGE_EVAL_API_KEY")
            if not key or not args.model:
                p.error("--openai-url needs --model and JUDGE_EVAL_API_KEY in the environment")

            def call(system: str, prompt: str) -> Reply:
                return openai_call(args.openai_url, args.model, key, system, prompt,
                                   args.max_tokens, args.timeout)
            judge = args.model
        started = time.monotonic()
        try:
            said, records, usage = run(cases, call, args.batch, args.save_raw)
        except Refused as e:
            why = (f"the {'Claude CLI' if args.claude else 'endpoint'} reported an "
                   f"authentication or usage-limit error: {e}")
            result = {"judge": f"{judge} {PROMPT_VERSION}", "error": why}
            if args.out:
                args.out.write_text(json.dumps(result, ensure_ascii=False, indent=1),
                                    encoding="utf-8")
            if args.summary_md:
                with args.summary_md.open("a", encoding="utf-8") as f:
                    f.write(summary_md(result))
            print(f"judge eval: stopped: {why}", file=sys.stderr)
            return 2
        usage["wall_seconds"] = round(time.monotonic() - started)
        unscored = {"failed": usage["cases_failed"], "dropped": usage["cases_dropped"]}
        result = {"judge": f"{usage['model'] or judge} {PROMPT_VERSION}", "usage": usage,
                  "scores": score(cases, said, unscored), "hint_recall_e5_small": 0.886}
    missed = gate(result["scores"]) if args.gate else []
    if args.gate:
        result["gate"] = {"passed": not missed, "missed": missed}
    print(json.dumps(result, indent=2, ensure_ascii=False))
    if args.out:
        args.out.write_text(json.dumps({**result, **({"cases": records} if records else {})},
                                       ensure_ascii=False, indent=1), encoding="utf-8")
    if args.summary_md:
        with args.summary_md.open("a", encoding="utf-8") as f:
            f.write(summary_md(result))
    for what in missed:
        print(f"judge eval: gate missed: {what}", file=sys.stderr)
    return 1 if missed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
