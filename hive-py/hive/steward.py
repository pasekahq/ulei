"""Read-only board context, model decisions and cancellation safety checks."""
import json
import re
import sys
import time
import unicodedata
import uuid

from . import c4g

DAY = 86400
MAX_INPUT_CHARS = 1_048_576


def norm(value):
    return ' '.join(unicodedata.normalize('NFC', value or '').casefold().split())


def cancellable(c):
    return c['state'] not in ('running', 'check', 'review', 'done', 'cancelled') and not (
        c['state'] == 'blocked' and c['wait_reason'] is None)


def duplicate(a, b):
    # Equal text on distinct series periods is intentional work. Compare series
    # definitions only when cadence, bounds and start agree as well.
    scope = ('kind', 'series_every', 'series_max', 'series_until', 'series_overlap', 'not_before',
             'series_cron', 'series_tz', 'series_misfire', 'script', 'script_timeout', 'deliver')
    if any(a[k] != b[k] for k in scope):
        return None
    if a['idem_key'] and a['idem_key'] == b['idem_key']:
        return 'idem'
    if norm(a['goal']) and norm(a['goal']) == norm(b['goal']):
        return 'goal'
    # A reused short title alone does not prove duplicate work.
    if (norm(a['title']) and norm(a['title']) == norm(b['title'])
            and a['acceptance'] == b['acceptance'] and a['body'] == b['body']):
        return 'title+acceptance+body'
    return None


def plan(conn, cfg, now=None):
    now = time.time() if now is None else now
    cards = [dict(r) for r in conn.execute('SELECT * FROM cards ORDER BY created_at,id')]
    instances = {r[0] for r in conn.execute("SELECT card_id FROM links WHERE kind='series'")}
    live = [c for c in cards if cancellable(c) and c['id'] not in instances and c['kind'] in ('task', 'recurring')]
    cancellations, keepers = [], []
    for c in live:
        match = next(((older, duplicate(older, c)) for older in keepers if duplicate(older, c)), None)
        reason = None
        if match:
            older, key = match
            reason = f"steward: duplicate {key}; keep {older['id']}"
        elif c['recheck_every'] is not None and c['state'] in ('created', 'blocked'):
            if c['recheck_until'] is not None and now >= c['recheck_until']:
                reason = 'steward: obsolete recheck; deadline expired'
            elif c['recheck_max'] is not None and c['recheck_attempts'] >= c['recheck_max']:
                reason = 'steward: obsolete recheck; attempts exhausted'
        if reason and safe(c, now, cfg['owner_actor']):
            cancellations.append({'id': c['id'], 'version': c['version'], 'reason': reason})
        else:
            keepers.append(c)
    return {'at': now, 'cancel': cancellations, 'propose': []}


def safe(card, now, owner):
    """Cancellable by the steward: not active or final, and not a card of `owner` (config owner_actor) younger than
    a day."""
    return card['state'] not in ('running', 'check', 'review', 'done', 'cancelled') and not (
        card['created_by'] == owner and now - card['created_at'] < DAY)


def board(conn, cfg, now=None):
    now = time.time() if now is None else now
    cards = []
    for row in conn.execute("SELECT * FROM cards WHERE state NOT IN ('done','cancelled') ORDER BY created_at,id"):
        c = dict(row)
        c['can_cancel'] = safe(c, now, cfg['owner_actor'])
        c['links'] = [dict(link) for link in conn.execute(
            'SELECT card_id,kind,target_id,optional FROM links WHERE card_id=? OR target_id=?', (c['id'], c['id']))]
        c['events'] = [dict(e) for e in conn.execute(
            'SELECT at,kind,actor,data FROM events WHERE card_id=? ORDER BY id DESC LIMIT 6', (c['id'],))][::-1]
        cards.append(c)
    return {'at': now, 'cards': cards, 'exact_hints': plan(conn, cfg, now)['cancel']}


def prompt(cfg, context):
    """The configured steward prompt (`texts.steward_prompt`) followed by the board JSON."""
    return cfg['texts']['steward_prompt'] + json.dumps(context, ensure_ascii=False)


class ModelResponseError(ValueError):
    """The model answered, but no JSON object could be recovered from the answer."""

    def __init__(self, message, raw=''):
        super().__init__(message)
        self.raw = raw


JSON_ONLY = ('\n\nНАПОМИНАНИЕ: предыдущий ответ не разобран. Верни только один JSON-объект '
             '{"cancel":[...],"decisions":[...]} без markdown-ограды, пояснений и текста до или после.')
FENCE = re.compile(r'```[A-Za-z0-9_-]*[ \t]*\r?\n?(.*?)```', re.S)
SECRET = re.compile(r'(?i)(sk-[A-Za-z0-9_-]{8,}|gh[pousr]_[A-Za-z0-9]{8,}|github_pat_[A-Za-z0-9_]{8,}'
                    r'|xox[abpr]-[A-Za-z0-9-]{8,}|\b\d{6,}:[A-Za-z0-9_-]{30,}'
                    r'|(?:api[_-]?key|token|secret|password|authorization)["\']?\s*[:=]?\s*(?:bearer\s+)?["\']?[^\s"\',}]{6,}|bearer\s+[^\s"\',}]{6,})')


def select_context(cfg, context):
    """Keep protected link components, then whole ordinary components within the retry budget.
    The original snapshot remains the authority for validation; no card fields are truncated.
    """
    cards = {c['id']: dict(c, can_cancel=safe(c, context['at'], cfg['owner_actor'])) for c in context['cards']}
    ordered = sorted(cards, key=lambda cid: (cards[cid]['created_at'], cid))
    adjacent = {cid: set() for cid in cards}
    for c in cards.values():
        for link in c['links']:
            a, b = link['card_id'], link['target_id']
            if a in cards and b in cards:
                adjacent[a].add(b)
                adjacent[b].add(a)
    groups, visited = [], set()
    for cid in ordered:
        if cid in visited:
            continue
        group, pending = set(), [cid]
        visited.add(cid)
        while pending:
            current = pending.pop()
            group.add(current)
            for other in adjacent[current] - visited:
                visited.add(other)
                pending.append(other)
        groups.append(group)

    total = len(cards)
    coverage = dict(total_cards=total, selected_cards=total, eligible_cards=total,
                    omitted_cards=total, partial=False)
    selected = dict(at=context['at'], cards=[], exact_hints=[], selection=coverage)
    # Maximum-width counts and list separators reserve space without repeatedly serializing the board.
    used = len(prompt(cfg, selected)) + len(JSON_ONLY)
    costs = {cid: len(json.dumps(cards[cid], ensure_ascii=False)) + 2 for cid in cards}
    for hint in context['exact_hints']:
        if hint['id'] in costs:
            costs[hint['id']] += len(json.dumps(hint, ensure_ascii=False)) + 2
    required = set().union(*(g for g in groups if any(not cards[c]['can_cancel'] for c in g)))
    used += sum(costs[cid] for cid in required)
    if used > MAX_INPUT_CHARS:
        raise ValueError('steward mandatory safety context exceeds model input limit')
    included = set(required)
    for group in groups:
        if group <= required:
            continue
        cost = sum(costs[cid] for cid in group)
        if used + cost <= MAX_INPUT_CHARS:
            included.update(group)
            used += cost
    selected['cards'] = [cards[cid] for cid in ordered if cid in included]
    selected['exact_hints'] = [h for h in context['exact_hints'] if h['id'] in included]
    coverage.update(selected_cards=len(included), omitted_cards=total - len(included),
                    eligible_cards=sum(cards[c]['can_cancel'] for c in included), partial=len(included) < total)
    if len(prompt(cfg, selected)) + len(JSON_ONLY) > MAX_INPUT_CHARS:
        raise ValueError('steward selected context exceeds model input limit')
    return selected


def redact(text, limit=2000):
    """Bounded, secret-scrubbed excerpt of a raw model answer for logs and alerts."""
    text = SECRET.sub('[REDACTED]', text if isinstance(text, str) else repr(text))
    return text if len(text) <= limit else text[:limit] + f'… [+{len(text) - limit} chars]'


def parse_answer(text):
    """Recover the first JSON object from a model answer: tolerate markdown fences and prose around it."""
    if not isinstance(text, str) or not text.strip():
        raise ModelResponseError('empty model answer', text if isinstance(text, str) else repr(text))
    decoder = json.JSONDecoder()
    found = []
    for chunk in [m.group(1) for m in FENCE.finditer(text)] + [text]:
        start = chunk.find('{')
        while start != -1:
            try:
                value, _ = decoder.raw_decode(chunk, start)
            except json.JSONDecodeError:
                start = chunk.find('{', start + 1)
                continue
            if isinstance(value, dict):
                if {'cancel', 'decisions'} <= set(value):
                    return value  # Prefer the plan over an illustrative object in prose.
                found.append(value)
            start = chunk.find('{', start + 1)
    if found:
        return found[0]
    raise ModelResponseError('no JSON object in model answer', text)


class ModelFailure(ValueError):
    """A steward C4G turn ended in a `c4g.Failure`; the message starts with its error class."""


def _ask(cfg, text):
    sc = cfg['steward']
    body = c4g.request(sc['route'], text, sc['max_tokens'], f'steward-{uuid.uuid4().hex}')
    outcome = c4g.turn(cfg['gateway_socket'], body, sc['timeout_s'])
    if isinstance(outcome, c4g.Failure):
        raise ModelFailure(f'{outcome.cls}: {outcome.message}')
    return outcome.text


def query_model(cfg, context, log=None):
    """One C4G turn on the `[steward] route` alias; on an unparseable answer log it (redacted) and send one more
    turn with a JSON-only reminder. A failed turn raises ModelFailure."""
    log = log or (lambda line: print(line, file=sys.stderr, flush=True))
    text = prompt(cfg, select_context(cfg, context))
    for attempt in (1, 2):
        try:
            return parse_answer(_ask(cfg, text if attempt == 1 else text + JSON_ONLY))
        except ModelResponseError as e:
            log(f'steward: attempt {attempt}/2: {e}; raw answer: {redact(e.raw)!r}')
            if attempt == 2:
                raise ModelResponseError(f'{e} after a JSON-only retry', e.raw) from None


def model_plan(cfg, context, answer):
    """Validate the entire response before permitting any mutation."""
    cards = {c['id']: c for c in context['cards']}
    selected = select_context(cfg, context)
    shown = {c['id'] for c in selected['cards']}
    if not isinstance(answer, dict) or set(answer) != {'cancel', 'decisions'}:
        raise ValueError('expected cancel and decisions')
    if not isinstance(answer['cancel'], list) or not isinstance(answer['decisions'], list):
        raise ValueError('expected lists')
    result = {'at': context['at'], 'cancel': [], 'propose': [], 'selection': selected['selection']}
    seen = set()
    for kind, rows in answer.items():
        for row in rows:
            if (not isinstance(row, dict) or not isinstance(row.get('id'), str)
                    or row['id'] not in cards or row['id'] in seen):
                raise ValueError('unknown or repeated card')
            seen.add(row['id'])
            reason = row.get('reason')
            if not isinstance(reason, str) or not reason.strip() or len(reason) > 1000 or '\n' in reason or '\r' in reason:
                raise ValueError('reason must be one nonempty line, at most 1000 characters')
            c = cards[row['id']]
            if c['id'] not in shown:
                raise ValueError(f"card omitted from model selection {c['id']}")
            if kind == 'cancel':
                if not safe(c, context['at'], cfg['owner_actor']):
                    raise ValueError(f"protected card {c['id']}")
                keep = row.get('keep_id')
                if keep is not None and (not isinstance(keep, str) or keep not in shown or keep == c['id']):
                    raise ValueError('invalid keeper')
                result['cancel'].append(dict(id=c['id'], version=c['version'], reason=reason, keep_id=keep,
                                             keep_version=cards[keep]['version'] if keep else None))
            else:
                if row.get('action') not in ('отменить', 'запустить', 'оставить'):
                    raise ValueError('invalid decision action')
                result['propose'].append(dict(id=c['id'], action=row['action'], reason=reason))
    cancelled = {r['id'] for r in result['cancel']}
    if any(r['keep_id'] in cancelled for r in result['cancel']):
        raise ValueError('keeper also cancelled')
    if len(result['propose']) > 5:
        raise ValueError('more than five owner decisions')
    return result


def digest(result):
    return '\n'.join(f"- {p['id']}: {p['action']} — {p['reason']}" for p in result['propose'][:5])
