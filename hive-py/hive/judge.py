"""One-shot judge of a head card: one C4G turn on the route alias the config maps from the card's step.

The answer's first `VERDICT:` line gives `done`, or `rework` followed by the reason.
"""
import re
import uuid

from . import c4g

REPORT_CHARS = 12000
GATE_CHARS = 2000
REVIEW_CHARS = 4000
VERDICT_RE = re.compile(r'VERDICT:\s*(done|rework)\b\W*(.*)$', re.I)
SYSTEM = '\n'.join([
    'You are the JUDGE of a finished task. Compare the result in the worker report with the task: its goal, its',
    'acceptance command (it already passed) and its judge criteria, and with the review if there is one.',
    'You cannot run commands; decide from the text you are given.',
    'The FIRST line of your answer is exactly `VERDICT: done` or `VERDICT: rework`.',
    'After `VERDICT: rework`, the next lines say what is missing or wrong and what must change; the next worker',
    'reads them as its instructions. Ask for rework only for concrete defects against the task.',
])


def clip(text, limit):
    """`text` cut to `limit` characters, keeping its start and its end."""
    if len(text) <= limit:
        return text
    half = limit // 2
    return text[:half] + f'\n[... {len(text) - 2 * half} characters cut ...]\n' + text[-half:]


def prompt(card, report, gate, review):
    """The task, the worker report, the gate output and, if there was a review, its verdict and text."""
    parts = [f"# Task {card['id']}: {card['title']}", '', '## Goal', card['goal']]
    if card['body']:
        parts += ['', '## Details', card['body']]
    parts += ['', '## Acceptance command', '```', card['acceptance'], '```']
    if card['judge_criteria']:
        parts += ['', '## Judge criteria', '```text', card['judge_criteria'], '```']
    parts += ['', f"## Worker report (round {card['round']})", clip(report or '(no report)', REPORT_CHARS),
              '', f"## Mechanical gate: exit {gate['exit_code']}", clip(gate['output'] or '(no output)', GATE_CHARS)]
    if review is not None:
        parts += ['', f"## Review: VERDICT {review['verdict']}", clip(review['report'] or '(no text)', REVIEW_CHARS)]
    return '\n'.join(parts)


def verdict(text):
    """('done', '') or ('rework', reason) from the first VERDICT line; None without one or for a rework without a
    reason."""
    lines = (text or '').splitlines()
    for i, line in enumerate(lines):
        m = VERDICT_RE.match(line.strip().lstrip('*`# '))
        if not m:
            continue
        if m.group(1).lower() == 'done':
            return 'done', ''
        reason = '\n'.join([m.group(2)] + lines[i + 1:]).strip()
        return ('rework', reason) if reason else None
    return None


def ask(cfg, card, report, gate, review):
    """One C4G turn for this judgement, carrying the card's tag if it has one: c4g.Text or c4g.Failure."""
    jc = cfg['judge']
    body = c4g.request(jc['routes'][card['step']], prompt(card, report, gate, review), jc['max_tokens'],
                       f'judge-{uuid.uuid4().hex}', system=SYSTEM, tag=card['tag'])
    return c4g.turn(cfg['gateway_socket'], body, jc['timeout_s'])
