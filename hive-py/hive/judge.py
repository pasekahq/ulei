"""One-shot judge of a head card: one C4G turn on the route alias the config maps from the card's step.

The answer's first `VERDICT:` line gives `done`, optionally followed by notes, or `rework` or `disputed` followed by the
reason. A rework is only for missing or wrong work: at most 3 essential defects, each naming the criterion it fails,
then minor gaps under `Notes:`, which go to the author and not to the next worker. Gaps in how the report presents
finished work are notes on a `done`, which start no round. `disputed` is no failure: the worker's evidence says the task itself is wrong, and the card's author
decides.
"""
import re
import uuid

from . import c4g

REPORT_CHARS = 12000
GATE_CHARS = 2000
REVIEW_CHARS = 4000
VERDICT_RE = re.compile(r'VERDICT:\s*(done|rework|disputed)\b\W*(.*)$', re.I)
NOTES_RE = re.compile(r'[#*_ ]*notes\b[*_ ]*(?:\([^)]*\))?[*_ ]*(?::[*_ ]*(.*)|$)', re.I)
SYSTEM = '\n'.join([
    'You are the JUDGE of a finished task. Compare the result in the worker report with the task: its goal, its',
    'acceptance command (it already passed) and its judge criteria, and with the review if there is one.',
    'You cannot run commands; decide from the text you are given.',
    'Judge the result, not the artifacts: a PR, a test or a file the task names is the means of its goal, not the',
    'goal. A worker who shows, by a reproduction on the layer the user sees, that the goal already holds and the',
    'artifact is not needed has met the goal. A worker who skips an artifact by reading code alone has not.',
    'Judge substance, not form. A rework costs the owner another round of waiting; ask for one when the work itself',
    'falls short: the goal is not met, the result is wrong, a claim the owner will act on has no evidence, or work a',
    'criterion requires was not done (a change of code, configuration or docs, a test, a check that proves the change',
    'works such as a test turn or a measurement, a merged PR, a part of the answer the goal asks for), or a rule of',
    'the task was broken.',
    'A record about the work (an issue, a ref, a link) is required work when the goal asks for it or a rework is due',
    'anyway. Next to a finished and correct deliverable, a missing record is a note that names it, for the author.',
    'For a change (code, configuration, docs, a PR), every change, test and doc the criteria require is required work.',
    'For a question or a research, the answer to what the owner asked is the goal, and an answer that covers only part',
    'of what the goal asks about is not done; but an item that widens the question is a note even when the criteria',
    'list it: other subjects next to the one the owner asked about, the CI of an already merged PR, extra statistics,',
    'extra context.',
    'A gap only in how the report presents finished work is a note, not a rework: a list instead of a table, wording,',
    'length, order, missing headings, a statement left implicit that the evidence already shows, raw command output',
    'next to a result with its source.',
    "The owner's clarifications (quoted in the report or in the details) outrank the criteria: when the owner asked",
    'for speed or a minimal change, everything beyond the deliverable they name (records, tables, statements) is a note.',
    'Before a rework, ask whether the owner would wait another round for the missing items. When the deliverable is',
    'done and correct and only notes are missing, the answer is no: `VERDICT: done` with the notes.',
    "Do not accept a worker's reason for skipping required work unless its evidence shows the work is not needed.",
    'A review FAIL is weighed by the same rule: a FAIL that lists only notes is `VERDICT: done` with the notes, and',
    'the items under its `Notes` heading are notes.',
    'The FIRST line of your answer is exactly `VERDICT: done`, `VERDICT: rework` or `VERDICT: disputed`.',
    'After `VERDICT: done`, you may add short notes on minor gaps for the card author; they start no new round.',
    'After `VERDICT: rework`, the next lines are a numbered list of at most 3 essential defects. Each item names the',
    'criterion, the part of the goal or the rule it fails, then says what is missing or wrong and what must change.',
    'The next worker reads this list as its instructions, so nothing minor goes in it. With more than 3 essential',
    'defects, name the 3 that matter most.',
    'Minor gaps go after the list, under a line `Notes:`, one per line. The notes go to the card author, not to the',
    'next worker, and they are never a reason for the rework.',
    'Answer `VERDICT: disputed` when the dispute is about the task itself, not the work: the worker reports',
    '`RESULT: disputed`, or its evidence says the premise, a required artifact or the criteria are wrong and the',
    'task text cannot settle it. Do not fail it and do not ask for rework: the card goes to its author, who decides.',
    'After `VERDICT: disputed`, the next lines state in two or three sentences what is disputed and the evidence on',
    'each side, for the author.',
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


def split_notes(text):
    """(instructions, notes) of a rework or a review: the text before its first line that opens the notes (`Notes:`,
    `## Notes`, `**Notes (minor):** first note`), and the notes from that line on; notes is '' without such a line."""
    lines = (text or '').splitlines()
    for i, line in enumerate(lines):
        m = NOTES_RE.match(line.strip())
        if m:
            return '\n'.join(lines[:i]).strip(), '\n'.join([m.group(1) or ''] + lines[i + 1:]).strip()
    return (text or '').strip(), ''


def verdict(text):
    """('done', notes), ('rework', reason) or ('disputed', reason) from the first VERDICT line, `notes` being the text
    after a `done` ('' without any); None without a VERDICT line or for a rework or a dispute without a reason."""
    lines = (text or '').splitlines()
    for i, line in enumerate(lines):
        m = VERDICT_RE.match(line.strip().lstrip('*`# '))
        if not m:
            continue
        reason = '\n'.join([m.group(2)] + lines[i + 1:]).strip()
        if m.group(1).lower() == 'done':
            return 'done', reason
        return (m.group(1).lower(), reason) if reason else None
    return None


def ask(cfg, card, report, gate, review):
    """One C4G turn for this judgement, carrying the card's tag if it has one: c4g.Text or c4g.Failure."""
    jc = cfg['judge']
    body = c4g.request(jc['routes'][card['step']], prompt(card, report, gate, review), jc['max_tokens'],
                       f'judge-{uuid.uuid4().hex}', system=SYSTEM, tag=card['tag'])
    return c4g.turn(cfg['gateway_socket'], body, jc['timeout_s'])
