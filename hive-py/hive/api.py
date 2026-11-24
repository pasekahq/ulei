"""Library API: create / dependencies / epics / get / list / wait. The CLI (scripts/hive.py) is a thin shell over this.

`create()` returns the card id synchronously; `wait()` blocks up to a timeout for a
terminal state. A future fast lane (retrieval cards) reuses both: create + wait(5s),
otherwise the result arrives later as an event.
"""
import json
import math
import re
import secrets
import sqlite3
import subprocess
import time
from pathlib import Path

from . import store
from .classification import CLASSES, legacy_class

KINDS = {'task': 'task', 'epic': 'epic', 'recurring': 'recurring'}   # kind -> lane. Only 'task' launches heads.
ACCEPTANCE_KINDS = ('command', 'judge')
DENY_TOOLS = ('browser', 'network', 'write')
WAIT_DEPENDENCY = 'dependency'
WAIT_CHILDREN = 'children'
WAIT_SCHEDULE = 'schedule'
WAITS = {'epic': WAIT_CHILDREN, 'recurring': WAIT_SCHEDULE}   # kind -> the wait it starts and stays in
OVERLAP = ('skip', 'queue')
NO_HEADS = ('epic', 'recurring')
LINKABLE_STATES = ('created', 'queued', 'blocked')
ON_UNAVAILABLE = ('wait', 'fallback')
VAGUE = re.compile(r'^\s*(готово|сделано|done|ok|ок|todo|tbd|-)\s*$', re.I)


class CardError(ValueError):
    pass


def normalize_deny_tools(deny_tools):
    """Return a canonical tool-denial policy or reject an invalid collection."""
    if not isinstance(deny_tools, (list, tuple)):
        raise CardError('deny tools must be a list or tuple')
    if any(not isinstance(tool, str) or tool not in DENY_TOOLS for tool in deny_tools):
        raise CardError(f"deny tools must be drawn from {', '.join(DENY_TOOLS)}")
    if len(set(deny_tools)) != len(deny_tools):
        raise CardError('deny tools must not contain duplicates')
    requested = set(deny_tools)
    return [tool for tool in DENY_TOOLS if tool in requested]


def normalize_recheck_policy(recheck_every, recheck_until, recheck_max, not_before, now):
    """Return a finite recheck policy and its initial due time, or reject its inputs."""
    values = (recheck_every, recheck_until, recheck_max)
    if all(value is None for value in values):
        return None
    if recheck_every is None:
        raise CardError('recheck_every is required when setting a recheck policy')
    if isinstance(recheck_every, bool) or not isinstance(recheck_every, (int, float)) or not math.isfinite(recheck_every) or recheck_every <= 0:
        raise CardError('recheck_every must be a positive number of seconds')
    if recheck_until is not None:
        if isinstance(recheck_until, bool) or not isinstance(recheck_until, (int, float)) or not math.isfinite(recheck_until):
            raise CardError('recheck_until must be an absolute timestamp')
        if recheck_until <= now:
            raise CardError('recheck_until must be in the future')
    if recheck_max is not None:
        if isinstance(recheck_max, bool) or not isinstance(recheck_max, int) or recheck_max <= 0:
            raise CardError('recheck_max must be a positive integer')
    if recheck_until is None and recheck_max is None:
        raise CardError('recheck policy requires recheck_until or recheck_max')
    due = now + recheck_every
    if recheck_until is not None and due >= recheck_until:
        raise CardError('first recheck must be before recheck_until')
    if not_before is not None and recheck_until is not None and not_before >= recheck_until:
        raise CardError('not_before must be before recheck_until')
    return {'every': float(recheck_every), 'until': recheck_until, 'max': recheck_max,
            'due': due, 'attempts': 0}


def validate_assignment(assignee, on_unavailable, executors):
    """Reject an assignee that is not a configured executor and an unknown unavailability policy."""
    if on_unavailable not in ON_UNAVAILABLE:
        raise CardError(f"on_unavailable must be one of {', '.join(ON_UNAVAILABLE)}")
    if assignee is not None and assignee not in executors:
        raise CardError(f"unknown executor {assignee!r}; configured: {', '.join(executors) or 'none'}")


def validate_external(cfg, executor, kind, assignee, model, effort, panel, task_type):
    """Reject an external executor that is not configured, a head pin beside it, a card kind that runs no worker
    and a task type the executor refuses."""
    configured = cfg.get('executors', {})
    if executor not in configured:
        raise CardError(f"unknown executor {executor!r}; configured external executors: {', '.join(configured) or 'none'}")
    if task_type.casefold() in {t.casefold() for t in configured[executor].get('refuse_task_types', [])}:
        raise CardError(f'external executor {executor!r} refuses task type {task_type!r}: the dots-delegation skill '
                        'keeps coding off dots (it goes to Codex and spends its limits)')
    if any(v is not None for v in (assignee, model, effort, panel)):
        raise CardError(f'external executor {executor!r} takes no assignee, model, effort or panel')
    if kind != 'task':
        raise CardError(f'external executor {executor!r} runs task cards only; {kind} refused')


def normalize_model_pin(cfg, model, effort, step, assignee):
    """Resolve a model pin against the loaded ladder, including its vendor and step-specific effort."""
    matches = [(s, vendor, head) for s, vendors in cfg['ladder'].items()
               for vendor, head in vendors.items() if head['model'] == model]
    if not matches:
        raise CardError(f'unknown model {model!r}; model must be present in the configured ladder')
    vendors = {vendor for _, vendor, _ in matches}
    if len(vendors) != 1:
        raise CardError(f'model {model!r} has ambiguous ladder vendors')
    vendor = next(iter(vendors))
    if assignee is not None and assignee != vendor:
        raise CardError(f'model {model!r} pins vendor {vendor}; assignee {assignee!r} conflicts')
    if effort is None:
        effort = next((head['effort'] for s, _, head in matches if s == int(step)), None)
        if effort is None:
            raise CardError(f'model {model!r} has no pairing at step {step}; specify --effort')
    efforts = {head['effort'] for ladder in cfg['ladder'].values()
               for v, head in ladder.items() if v == vendor}
    if not isinstance(effort, str) or effort not in efforts:
        raise CardError(f'effort {effort!r} is not used by ladder vendor {vendor}; configured: {", ".join(sorted(efforts))}')
    return vendor, effort


def normalize_panel(cfg, panel, step):
    """Resolve distinct runnable pins across vendors with file-safe model-name labels.
    Collisions use effort, then a suffix assigned in sorted pin order."""
    from . import heads
    runnable = heads.executors(cfg)
    if panel is True:
        panel = [head['model'] + ':' + head['effort']
                 for vendor, head in cfg['ladder'][int(step)].items() if vendor in runnable]
    elif isinstance(panel, str):
        panel = panel.split(',')
    if not isinstance(panel, (list, tuple)) or not panel:
        raise CardError('panel requires a member list or the default ladder panel')
    members = []
    for item in panel:
        if not isinstance(item, str) or not item.strip() or item.count(':') > 1:
            raise CardError('panel members must be model or model:effort')
        model, sep, effort = item.strip().partition(':')
        vendor, effort = normalize_model_pin(cfg, model, effort if sep else None, step, None)
        if vendor not in runnable:
            raise CardError(f'panel member {model!r} has no runnable executor {vendor!r}')
        name = re.sub(r'^(?:claude|gpt)-', '', model.lower())
        label = re.sub(r'[^a-z]+', '-', name).strip('-') or 'model'
        members.append({'label': label, 'vendor': vendor, 'model': model, 'effort': effort})
    pins = {(m['vendor'], m['model'], m['effort']) for m in members}
    if len(pins) != len(members) or len(members) < 2 or len({m['vendor'] for m in members}) < 2:
        raise CardError('panel requires at least two distinct members spanning at least two vendors')
    labels = [m['label'] for m in members]
    used = set(labels)
    for member in sorted(members, key=lambda m: (m['vendor'], m['model'], m['effort'])):
        if labels.count(member['label']) == 1:
            continue
        effort_label = re.sub(r'[^a-z0-9]+', '-', member['effort'].lower()).strip('-') or 'effort'
        base = f"{member['label']}-{effort_label}"
        label, suffix = base, 2
        while label in used:
            label = f'{base}-{suffix}'
            suffix += 1
        member['label'] = label
        used.add(label)
    return members


def empty_acceptance(command):
    """Report existence proves no outcome; require independent review."""
    import shlex
    try:
        words = shlex.split((command or '').strip().rstrip(';'))
    except ValueError:
        return False
    return words in (['test', '-s', 'REPORT.md'], ['test', '-s', './REPORT.md'],
                     ['[', '-s', 'REPORT.md', ']'], ['[', '-s', './REPORT.md', ']'])


def owner_card(card):
    return card['card_class'] == 'owner_task'


ADMISSION_CLASSES = ('urgent', 'owner', 'followup', 'priority', 'background')


def admission_class(card, cfg):
    """The card's admission class; the dispatcher launches classes in ADMISSION_CLASSES order in every pool."""
    adm = cfg['admission']
    def sourced(sources):
        return card['source'] in sources or any(card['source'] == s + ':conditional' for s in sources)
    if owner_card(card) and card['step'] <= adm['urgent_max_step'] and card['state'] in ('queued', 'review'):
        return 'urgent'
    if sourced(adm['urgent_sources']):
        return 'urgent'
    if owner_card(card):
        return 'owner'
    if card['card_class'] == 'followup':
        return 'followup'
    if sourced(cfg.get('resources', {}).get('priority_sources', [])):
        return 'priority'
    return 'background'


def validate(title, goal, acceptance, step, source, created_by, kind, acceptance_kind, judge_criteria, deny_tools, review=False):
    if empty_acceptance(acceptance) and not review:
        raise CardError("test -s REPORT.md requires review")
    if not (title or '').strip():
        raise CardError('title is required')
    if not (goal or '').strip():
        raise CardError('goal is required')
    if not (acceptance or '').strip() or VAGUE.match(acceptance):
        raise CardError('acceptance must be a shell command whose exit 0 means done')
    r = subprocess.run(['bash', '-n', '-c', acceptance], capture_output=True, text=True)
    if r.returncode != 0:
        raise CardError(f'acceptance is not valid bash: {r.stderr.strip()[:200]}')
    if int(step) not in range(1, 6):
        raise CardError('step must be 1..5')
    if not (source or '').strip():
        raise CardError('source is required (msg id / ledger id / issue / cron name / manual)')
    if not (created_by or '').strip():
        raise CardError('created_by is required (secretary / cron:<name> / the owner)')
    if kind not in KINDS:
        raise CardError(f'unknown kind {kind!r}')
    if acceptance_kind not in ACCEPTANCE_KINDS:
        raise CardError(f'unknown acceptance kind {acceptance_kind!r}')
    if acceptance_kind == 'judge':
        if not isinstance(judge_criteria, str) or not judge_criteria.strip():
            raise CardError('judge criteria must be non-blank text for judge acceptance')
    normalize_deny_tools(deny_tools)


def normalize_series(every, max_runs, until, overlap, not_before, now, cron=None, tz=None, misfire=None):
    """Validate a finite interval or optionally unbounded cron schedule."""
    if cron is None:
        if isinstance(every, bool) or not isinstance(every, (int, float)) or not math.isfinite(every) or every <= 0:
            raise CardError('every must be a positive number of seconds')
        if tz is not None or misfire is not None:
            raise CardError('tz and misfire require cron')
    elif every is not None:
        raise CardError('cron and every are mutually exclusive')
    if max_runs is not None and (isinstance(max_runs, bool) or not isinstance(max_runs, int) or max_runs <= 0):
        raise CardError('max_runs must be a positive integer')
    if until is not None:
        if isinstance(until, bool) or not isinstance(until, (int, float)) or not math.isfinite(until):
            raise CardError('until must be an absolute timestamp')
        if until <= now:
            raise CardError('until must be in the future')
    if cron is None and max_runs is None and until is None:
        raise CardError('a recurring card requires max_runs or until')
    overlap = 'skip' if overlap is None else overlap
    if overlap not in OVERLAP:
        raise CardError(f"overlap must be one of {', '.join(OVERLAP)}")
    start = now if not_before is None else not_before
    if until is not None and start >= until:
        raise CardError('the first period must start before until')
    extra = {}
    if cron is not None:
        from .schedule import next_cron
        from cronsim import CronSimError
        from zoneinfo import ZoneInfoNotFoundError
        if not isinstance(cron, str) or len(cron.split()) != 5:
            raise CardError('cron must have five fields')
        if misfire not in (None, 'skip', 'latest'):
            raise CardError('misfire must be skip or latest')
        try:
            due = next_cron(cron, tz, start - 1 if not_before is not None else start)
        except (ValueError, CronSimError, ZoneInfoNotFoundError, TypeError) as e:
            raise CardError('invalid cron expression or time zone') from e
        if until is not None and due >= until:
            raise CardError('the first cron occurrence must start before until')
        extra = dict(series_cron=cron, series_tz=tz, series_due=due, series_misfire=misfire or 'skip')
    return {**extra, 'series_every': float(every) if every is not None else None, 'series_max': max_runs, 'series_until': until, 'series_overlap': overlap,
            'series_next': 1, 'series_skipped': 0, 'not_before': start}


def period_start(series, period):
    if series['series_cron'] is not None:
        if period != series['series_next']:
            raise ValueError('cron period_start requires the persisted cursor')
        return series['series_due']
    return series['not_before'] + (period - 1) * series['series_every']


def series_open(series, created):
    """More periods may still come: not stopped, under max_runs and the next period starts before until."""
    until = series['series_until']
    return (series['series_stopped'] is None
            and (series['series_max'] is None or created < series['series_max'])
            and (until is None or period_start(series, series['series_next']) < until))


def due_period(series, now):
    """The latest period at or after series_next that has started by now and before until, or None."""
    until = series['series_until']
    if now < period_start(series, series['series_next']) or until is not None and now >= until:
        return None
    return math.floor((now - series['not_before']) / series['series_every']) + 1


def create(conn, *, title, goal, acceptance=None, step, source, created_by, review=False, body='',
           task_type='general', idem_key=None, kind='task', priority=None, not_before=None, card_id=None,
           acceptance_kind='command', judge_criteria=None, deny_tools=(), recheck_every=None,
           recheck_until=None, recheck_max=None, blocked_by=(), parent=None, optional=False,
           assignee=None, on_unavailable='wait', executors=(), every=None, max_runs=None, until=None,
           overlap=None, model=None, effort=None, cfg=None, panel=None, executor=None,
           cron=None, tz=None, misfire=None, start_window=None, script=None, script_timeout=None, deliver=None, gate_timeout=None,
           card_class=None, owner_visible=None, followup_of=None):
    """Return (card_id, created), inserting a validated card and its initial dependency or schedule waits atomically.
    A model pin resolves vendor and effort from cfg (or the loaded config); assignee alone uses executors.
    An external executor must be configured under [executors.*] in cfg and excludes every head pin.
    Idempotency is decided by the UNIQUE key inside the transaction."""
    if isinstance(blocked_by, str) or not isinstance(blocked_by, (list, tuple)):
        raise CardError('blocked_by must be a list or tuple of card ids')
    if optional and parent is None:
        raise CardError('optional applies only to a child created with a parent epic')
    if start_window is not None:
        if kind != 'recurring' or cron is None or script is None or misfire not in (None, 'skip'):
            raise CardError('start_window requires a recurring cron script with misfire=skip')
        from .schedule import parse_start_window
        try:
            parse_start_window(start_window)
        except ValueError as e:
            raise CardError(str(e)) from e
    if deliver == 'telegram':
        deliver = 'owner-inbox'  # legacy CLI spelling, secretary decides visibility
    if deliver not in (None, 'telegram', 'owner-inbox', 'local'):
        raise CardError('deliver must be telegram, owner-inbox or local')
    if script is not None:
        script = str(Path(script).expanduser().resolve())
        if not Path(script).is_file():
            raise CardError('script must be an existing file')
        from .script import command
        try:
            command(script)
        except ValueError as e:
            raise CardError(str(e)) from e
        if kind == 'epic' or any(v is not None for v in (executor, model, effort, panel, assignee)) or review or acceptance_kind != 'command' or recheck_every is not None:
            raise CardError('script takes no head, executor, review, epic or recheck policy')
        if script_timeout is None:
            script_timeout = 300
        if isinstance(script_timeout, bool) or not isinstance(script_timeout, (int, float)) or not math.isfinite(script_timeout) or script_timeout <= 0:
            raise CardError('script_timeout must be a positive number of seconds')
        acceptance = acceptance or 'true'
    elif script_timeout is not None:
        raise CardError('script_timeout requires script')
    if executor is not None:
        cfg = store.load_config() if cfg is None else cfg
        validate_external(cfg, executor, kind, assignee, model, effort, panel, task_type)
    members = None
    if panel is not None:
        if any(v is not None for v in (model, effort, assignee)) or review or kind != 'task':
            raise CardError('panel takes no model, effort, assignee, review, epic or recurring card')
        cfg = store.load_config() if cfg is None else cfg
        if int(step) not in range(1, 6):
            raise CardError('step must be 1..5')
        members = normalize_panel(cfg, panel, step)
    recheck = recheck_every is not None or recheck_until is not None or recheck_max is not None
    if kind == 'epic':
        if acceptance_kind != 'command':
            raise CardError('an epic takes a command acceptance only')
        if blocked_by:
            raise CardError('an epic cannot have blocked_by dependencies')
        if review or not_before is not None or recheck:
            raise CardError('an epic takes no review, not_before or recheck policy')
        if assignee is not None or model is not None or effort is not None:
            raise CardError('an epic runs no heads and takes no assignee, model or effort pin')
    if kind == 'recurring':
        if recheck:
            raise CardError('a recurring card takes no recheck policy')
        if blocked_by or parent is not None:
            raise CardError('a recurring card takes no blocked_by dependencies or parent epic')
    elif any(v is not None for v in (every, max_runs, until, overlap, cron, tz, misfire)):
        raise CardError('schedule flags apply only to a recurring card')
    if model is None and effort is not None:
        raise CardError('--effort requires --model')
    if model is not None:
        from . import heads
        cfg = store.load_config() if cfg is None else cfg
        assignee, effort = normalize_model_pin(cfg, model, effort, step, assignee)
        executors = heads.executors(cfg)
    validate_assignment(assignee, on_unavailable, executors)
    validate(title, goal, acceptance, step, source, created_by, kind, acceptance_kind, judge_criteria, deny_tools, review)
    if gate_timeout is not None and (isinstance(gate_timeout, bool) or not isinstance(gate_timeout, (int, float))
                                     or not math.isfinite(gate_timeout) or gate_timeout <= 0):
        raise CardError('gate_timeout must be a positive number of seconds')
    if followup_of is not None:
        ancestor = card_row(conn, followup_of)
        if card_class is None:
            card_class = ancestor['card_class']
        if owner_visible is None:
            owner_visible = bool(ancestor['owner_visible'])
    if card_class is None:
        card_class = legacy_class(source, series=kind == 'recurring')
    if card_class not in CLASSES:
        raise CardError('card_class required: use --kind owner_task|series_run|system|followup; unknown source')
    if kind == 'recurring' and card_class != 'series_run':
        raise CardError('recurring cards require series_run class')
    if owner_visible is None:
        owner_visible = card_class == 'owner_task'
    if not isinstance(owner_visible, bool):
        raise CardError('owner_visible must be a boolean')
    now = time.time()
    policy = normalize_recheck_policy(recheck_every, recheck_until, recheck_max, not_before, now)
    fields = {'not_before': not_before}
    if kind == 'recurring':
        if cron is not None and tz is None:
            cfg = store.load_config() if cfg is None else cfg
            tz = cfg['owner_tz']
        fields = normalize_series(every, max_runs, until, overlap, not_before, now, cron, tz, misfire)
        fields['series_start_window'] = start_window
    if policy:
        fields.update(recheck_every=policy['every'], recheck_until=policy['until'], recheck_max=policy['max'],
                      recheck_due=policy['due'])
    fields.update(card_class=card_class, owner_visible=int(owner_visible), kind=kind, lane=KINDS[kind], title=title.strip(), goal=goal.strip(), acceptance=acceptance.strip(),
                  acceptance_kind=acceptance_kind, judge_criteria=judge_criteria.strip() if judge_criteria else None,
                  deny_tools=json.dumps(normalize_deny_tools(deny_tools)), body=body or '', step=int(step),
                  review=1 if review else 0, task_type=task_type, source=source.strip(), created_by=created_by.strip(),
                  idem_key=idem_key, priority=int(priority) if priority is not None else (100 if card_class == 'owner_task' and kind == 'task' else 0), assignee=assignee, on_unavailable=on_unavailable,
                  model=model, effort=effort, panel=json.dumps(members) if members is not None else None,
                  executor=executor, script=script, script_timeout=script_timeout, gate_timeout=gate_timeout,
                  deliver=deliver or 'owner-inbox')
    with store.tx(conn):
        cid, created = insert_card(conn, card_id or 'h_' + secrets.token_hex(4), fields, blocked_by, parent, optional, now)
        if created and followup_of is not None:
            conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'followup',?,?)", (cid, followup_of, now))
        return cid, created


def insert_card(conn, cid, fields, blocked_by, parent, optional, now):
    """Inside a tx. Insert a validated card, link it and park it in its initial wait. Returns (card_id, created);
    an existing card with the same idem_key is returned with created False."""
    if fields['idem_key']:
        row = conn.execute('SELECT id FROM cards WHERE idem_key=?', (fields['idem_key'],)).fetchone()
        if row:
            return row['id'], False
    row = dict(fields, id=cid, state='created', round=0, recheck_attempts=0, created_at=now, updated_at=now)
    try:
        conn.execute(f"INSERT INTO cards({','.join(row)}) VALUES({','.join('?' * len(row))})", list(row.values()))
    except sqlite3.IntegrityError:
        # Unreachable under BEGIN IMMEDIATE for idem_key, kept as a second fence.
        found = conn.execute('SELECT id FROM cards WHERE idem_key=?', (fields['idem_key'],)).fetchone()
        if found:
            return found['id'], False
        raise
    kind, created_by = fields['kind'], fields['created_by']
    store.event(conn, cid, 'created', {'by': created_by, 'source': fields['source'], 'step': fields['step'],
                                      'review': bool(fields['review']), 'kind': kind, 'card_class': fields['card_class'], 'owner_visible': bool(fields['owner_visible']), 'assignee': fields['assignee'],
                                      'on_unavailable': fields['on_unavailable'], 'executor': fields['executor'],
                                      'model': fields['model'], 'effort': fields['effort'],
                                      'panel': json.loads(fields['panel']) if fields['panel'] else None}, actor=created_by)
    card = card_row(conn, cid)
    for blocker_id in dict.fromkeys(blocked_by):
        link_blocker(conn, card, blocker_id, created_by, now)
    if parent is not None:
        link_parent(conn, card, parent, optional, created_by, now)
    if kind in WAITS:
        if not store.cas_card(conn, card, 'blocked', wait_reason=WAITS[kind]):
            raise CardError(f'card {cid} changed concurrently')
        store.event(conn, cid, f'{WAITS[kind]}_wait', None, actor=created_by)
    else:
        wait_for_blockers(conn, card, created_by)
    return cid, True


def stop(conn, card_id, actor='secretary'):
    """Stop a recurring card: no new instances, existing ones untouched. False when it was already stopped or final."""
    with store.tx(conn):
        card = card_row(conn, card_id)
        if card['kind'] != 'recurring':
            raise CardError(f'card {card_id} is not a recurring card')
        if card['series_stopped'] is not None or store.is_final(card):
            return False
        if not store.cas_card(conn, card, card['state'], series_stopped=time.time()):
            raise CardError(f'card {card_id} changed concurrently')
        store.event(conn, card_id, 'series_stopped', None, actor=actor)
        return True


def card_row(conn, card_id):
    row = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    if row is None:
        raise CardError(f'unknown card {card_id!r}')
    return row


def link_blocker(conn, card, blocker_id, actor, now):
    """Inside a tx. Store `card` blocked_by `blocker_id`; False when the link already exists.
    Rejects an unlinkable card state, an unknown blocker, a self link and a link that closes a cycle."""
    if card['kind'] in NO_HEADS:
        raise CardError(f"card {card['id']} is {card['kind']}; it cannot have blocked_by dependencies")
    if card['state'] not in LINKABLE_STATES:
        raise CardError(f"card {card['id']} is {card['state']}; dependencies are added only in {', '.join(LINKABLE_STATES)}")
    if blocker_id == card['id']:
        raise CardError(f"card {card['id']} cannot block itself")
    card_row(conn, blocker_id)
    if conn.execute("SELECT 1 FROM links WHERE card_id=? AND kind='blocked_by' AND target_id=?",
                    (card['id'], blocker_id)).fetchone():
        return False
    cycle = conn.execute("""WITH RECURSIVE upstream(id) AS (
                              SELECT ? UNION SELECT l.target_id FROM links l JOIN upstream u ON l.card_id=u.id
                              WHERE l.kind='blocked_by')
                            SELECT 1 FROM upstream WHERE id=?""", (blocker_id, card['id'])).fetchone()
    if cycle:
        raise CardError(f"card {blocker_id} already depends on {card['id']}; the link would close a cycle")
    conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'blocked_by',?,?)",
                 (card['id'], blocker_id, now))
    store.event(conn, card['id'], 'dependency_added', {'blocker': blocker_id}, actor=actor)
    return True


def wait_for_blockers(conn, card, actor):
    """Inside a tx. Park a created or queued card in blocked while any of its blockers is not done."""
    if card['state'] not in ('created', 'queued'):
        return
    pending = [b['id'] for b in store.blockers(conn, card['id']) if b['state'] not in ('done', 'cancelled')]
    if not pending:
        return
    if not store.cas_card(conn, card, 'blocked', wait_reason=WAIT_DEPENDENCY, executor_wait=None):
        raise CardError(f"card {card['id']} changed concurrently")
    store.event(conn, card['id'], 'dependency_wait', {'blockers': pending}, actor=actor)


def add_blocker(conn, card_id, blocker_id, actor='secretary'):
    """Make `card_id` wait for `blocker_id` to be done. Returns False when the link already existed."""
    with store.tx(conn):
        card = card_row(conn, card_id)
        added = link_blocker(conn, card, blocker_id, actor, time.time())
        if added:
            wait_for_blockers(conn, card, actor)
        return added


def remove_blocker(conn, card_id, blocker_id, actor='secretary'):
    """Drop the dependency; the dispatcher unblocks the card on its next tick. False when there was none."""
    with store.tx(conn):
        card_row(conn, card_id)
        card_row(conn, blocker_id)
        cur = conn.execute("DELETE FROM links WHERE card_id=? AND kind='blocked_by' AND target_id=?", (card_id, blocker_id))
        if cur.rowcount:
            store.event(conn, card_id, 'dependency_removed', {'blocker': blocker_id}, actor=actor)
        return cur.rowcount == 1


def epic_row(conn, epic_id):
    """The epic's row; rejects an unknown card and a card that is not an epic."""
    epic = card_row(conn, epic_id)
    if epic['kind'] != 'epic':
        raise CardError(f'card {epic_id} is not an epic')
    return epic


def link_parent(conn, card, epic_id, optional, actor, now):
    """Inside a tx. Make `card` a child of `epic_id`; False when that exact link already exists.
    Rejects a non-epic or unknown parent, a recurring child, a self link, a second parent, a cycle and a done epic."""
    epic = epic_row(conn, epic_id)
    if card['kind'] == 'recurring':
        raise CardError(f"card {card['id']} is recurring; it cannot be a child of an epic")
    if epic_id == card['id']:
        raise CardError(f"card {card['id']} cannot be its own parent")
    current = store.parent(conn, card['id'])
    if current is not None and current['id'] != epic_id:
        raise CardError(f"card {card['id']} already belongs to epic {current['id']}")
    if current is not None and current['optional'] == bool(optional):
        return False
    if epic['state'] in ('done', 'cancelled'):
        raise CardError(f'epic {epic_id} is final; children are not added to it')
    cycle = conn.execute("""WITH RECURSIVE ancestors(id) AS (
                              SELECT ? UNION SELECT l.target_id FROM links l JOIN ancestors a ON l.card_id=a.id
                              WHERE l.kind='parent')
                            SELECT 1 FROM ancestors WHERE id=?""", (epic_id, card['id'])).fetchone()
    if cycle:
        raise CardError(f"epic {epic_id} already descends from {card['id']}; the link would close a cycle")
    data = {'child': card['id'], 'epic': epic_id, 'optional': bool(optional)}
    if current is None:
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at,optional) VALUES(?,'parent',?,?,?)",
                     (card['id'], epic_id, now, int(bool(optional))))
        kind = 'child_added'
    else:
        conn.execute("UPDATE links SET optional=? WHERE card_id=? AND kind='parent' AND target_id=?",
                     (int(bool(optional)), card['id'], epic_id))
        kind = 'child_changed'
    store.event(conn, card['id'], kind, data, actor=actor, at=now)
    store.event(conn, epic_id, kind, data, actor=actor, at=now)
    return True


def add_child(conn, card_id, epic_id, optional=False, actor='secretary'):
    """Attach `card_id` to `epic_id` (required unless `optional`). Returns False when nothing changed."""
    with store.tx(conn):
        return link_parent(conn, card_row(conn, card_id), epic_id, optional, actor, time.time())


def remove_child(conn, card_id, epic_id, actor='secretary'):
    """Detach `card_id` from `epic_id`. Returns False when it was not attached."""
    with store.tx(conn):
        card_row(conn, card_id)
        epic_row(conn, epic_id)
        cur = conn.execute("DELETE FROM links WHERE card_id=? AND kind='parent' AND target_id=?", (card_id, epic_id))
        if cur.rowcount:
            data = {'child': card_id, 'epic': epic_id}
            store.event(conn, card_id, 'child_removed', data, actor=actor)
            store.event(conn, epic_id, 'child_removed', data, actor=actor)
        return cur.rowcount == 1


def progress(children):
    """Required children of an epic: done/total plus the blocked ones split into waiting and failed."""
    required = [c for c in children if not c['optional']]
    blocked = [c for c in required if c['state'] == 'blocked']
    return {'done': sum(c['state'] == 'done' for c in required), 'total': len(required),
            'blocked_waiting': sum(not store.is_final(c) for c in blocked),
            'blocked_failed': sum(store.is_final(c) for c in blocked)}


def block_reasons(card, blockers, children):
    """Every current reason a blocked card is blocked: schedule, dependency and children waits and a failure are listed side by side."""
    if card['state'] != 'blocked':
        return []
    reasons = []
    if card['wait_reason'] == WAIT_SCHEDULE:
        reasons.append({'kind': WAIT_SCHEDULE})
    if card['wait_reason'] == WAIT_CHILDREN:
        reasons.append({'kind': WAIT_CHILDREN, 'pending': [c['id'] for c in children
                                                          if not c['optional'] and c['state'] not in ('done', 'cancelled')]})
    pending = [b['id'] for b in blockers if b['state'] not in ('done', 'cancelled')]
    if card['wait_reason'] == WAIT_DEPENDENCY or pending:
        reasons.append({'kind': WAIT_DEPENDENCY, 'blockers': pending})
    if card['block_reason'] is not None:
        reasons.append({'kind': 'failure', 'reason': card['block_reason']})
    return reasons


def release(conn, card_id, actor='secretary', at=None):
    """Lift a not_before hold on a 'created' card or one waiting in blocked (trigger happened). Not a state
    move: the dispatcher queues it once nothing else holds it. Returns True if the hold was changed.
    Rejects an epic and a recurring card: neither has a hold to lift."""
    with store.tx(conn):
        row = conn.execute('SELECT kind FROM cards WHERE id=?', (card_id,)).fetchone()
        if row is not None and row['kind'] in NO_HEADS:
            raise CardError(f"card {card_id} is {row['kind']}; it has no hold to release")
        cur = conn.execute("""UPDATE cards SET not_before=?, version=version+1, updated_at=?
                              WHERE id=? AND (state='created' OR state='blocked' AND wait_reason IS NOT NULL)""",
                           (at, time.time(), card_id))
        if cur.rowcount:
            store.event(conn, card_id, 'released', {'not_before': at}, actor=actor)
        return cur.rowcount == 1


def assign(conn, card_id, assignee, on_unavailable='wait', executors=(), actor='secretary'):
    """Set (or with None remove) the worker executor and its unavailability policy of a created, queued or
    waiting card. Returns False when both already hold those values; rejects an epic and any other state."""
    with store.tx(conn):
        card = card_row(conn, card_id)
        if card['script'] is not None:
            raise CardError(f'card {card_id} runs a script and takes no head assignment or steer')
        if card['panel'] is not None:
            raise CardError(f'card {card_id} is a panel; its members cannot be assigned')
        if card['executor'] is not None:
            raise CardError(f"card {card_id} runs on external executor {card['executor']}; it takes no assignee")
        if card['model'] is not None and assignee != card['assignee']:
            raise CardError(f"model pin {card['model']!r} requires assignee {card['assignee']}; cannot assign {assignee!r}")
        validate_assignment(assignee, on_unavailable, executors)
        if card['kind'] == 'epic':
            raise CardError(f'card {card_id} is an epic; an epic runs no heads and takes no assignee')
        if card['state'] not in LINKABLE_STATES or store.is_final(card):
            raise CardError(f"card {card_id} is {card['state']}; the assignee changes only in created, queued "
                            'or a waiting blocked state')
        if (card['assignee'], card['on_unavailable']) == (assignee, on_unavailable):
            return False
        if not store.cas_card(conn, card, card['state'], assignee=assignee, on_unavailable=on_unavailable,
                              executor_wait=None):
            raise CardError(f'card {card_id} changed concurrently')
        store.event(conn, card_id, 'assigned', {'assignee': assignee, 'on_unavailable': on_unavailable,
                                                'previous': {'assignee': card['assignee'],
                                                             'on_unavailable': card['on_unavailable']}}, actor=actor)
        return True


def steer(conn, card_id, text, actor):
    """Append a 'steer' event with the verbatim text to the card journal and return its id. The card row
    is untouched; the dispatcher delivers it (see steer_deliveries). Raises CardError on a blank text,
    blank actor, unknown card or an epic or recurring card, which no worker ever runs."""
    if not (text or '').strip():
        raise CardError('steer text is required')
    if not (actor or '').strip():
        raise CardError('steer actor is required')
    actor = actor.strip()
    with store.tx(conn):
        card = card_row(conn, card_id)
        if card['script'] is not None:
            raise CardError(f'card {card_id} runs a script and takes no head assignment or steer')
        if card['panel'] is not None:
            raise CardError(f'card {card_id} is a panel; panel members cannot receive a steer')
        if card['executor'] is not None:
            raise CardError(f"card {card_id} runs on external executor {card['executor']}; it cannot receive a steer")
        kind = card['kind']
        if kind in NO_HEADS:
            raise CardError(f'card {card_id} is {kind}; no worker receives a steer')
        store.event(conn, card_id, 'steer', {'text': text, 'by': actor}, actor=actor)
        return conn.execute('SELECT last_insert_rowid()').fetchone()[0]


def quotes(report, text):
    """A head acknowledges a steer by quoting its text verbatim in its report."""
    return text.strip() in (report or '')


def fresh_report(conn, run_id):
    """The run's report text when its runner captured a report file this run's head wrote, else None."""
    row = conn.execute("""SELECT r.report FROM runs r JOIN events e ON e.card_id=r.card_id AND e.kind='run_report'
                          AND json_extract(e.data, '$.run')=r.id AND json_extract(e.data, '$.source')='file'
                          WHERE r.id=?""", (run_id,)).fetchone()
    return row['report'] if row else None


def steer_deliveries(conn, card_id):
    """Each steer of the card, oldest first, with `run` (the worker run concerned) and `delivery`:
    pending (the next worker round gets it), pending-live (runner input pending), delivered-live
    (vendor acknowledged input), stopping (fallback stop), restarted (new head's prompt), handed_off
    (in an active worker's initial prompt), acknowledged (a worker report quotes
    it verbatim in a report it wrote itself), unacknowledged (the last worker that had it ended without quoting it) or undelivered
    (the card closed before any worker got it). `method` retains delivered-live/restarted after the
    report acknowledges it; `reason` explains a fallback. Old events need no migration."""
    card = conn.execute('SELECT state, round, wait_reason FROM cards WHERE id=?', (card_id,)).fetchone()
    claims = conn.execute("""SELECT e.id AS claim, r.id, r.status FROM events e JOIN runs r ON r.id=json_extract(e.data, '$.run')
                             WHERE e.card_id=? AND e.kind='claimed' AND json_extract(e.data, '$.role')='worker' ORDER BY e.id""",
                          (card_id,)).fetchall()
    active = conn.execute("""SELECT id, run_dir FROM runs WHERE card_id=? AND role='worker' AND round=? AND status IN ('starting','running')""",
                          (card_id, card['round'])).fetchone() if card['state'] == 'running' else None
    out = []
    reports = {r['id']: fresh_report(conn, r['id']) for r in claims}
    live = {r['id']: store.live_steers(conn, r['id']) for r in claims}
    restarts = [json.loads(e['data']) for e in conn.execute(
        "SELECT data FROM events WHERE card_id=? AND kind='steer_restart' ORDER BY id", (card_id,))]
    stopping = {json.loads(e['data'])['run'] for e in conn.execute(
        "SELECT data FROM events WHERE card_id=? AND kind='steer_stop'", (card_id,))}
    for s in store.steers(conn, card_id):
        got = [r for r in claims if r['claim'] > s['id'] or s['id'] in live[r['id']]]
        ack = next((r for r in got if quotes(reports[r['id']], s['text'])), None)
        if ack:
            delivery, run = 'acknowledged', ack['id']
        elif got:
            run = got[-1]['id']
            delivery = 'handed_off' if got[-1]['status'] in store.ACTIVE_RUN else 'unacknowledged'
        elif store.is_final(card):
            delivery, run = 'undelivered', None
        elif active:
            delivery, run = 'stopping', active['id']
        else:
            delivery, run = 'pending', None
        method, reason = None, None
        restarted = next((e for e in reversed(restarts) if s['id'] in e['steers']), None)
        if got and s['id'] in live[got[-1]['id']]:
            method = 'delivered-live'
        elif restarted:
            method, reason = 'restarted', restarted['reason']
        # Keep report acknowledgement distinct from the transport result.
        if delivery == 'handed_off' and method:
            delivery = method
        elif delivery == 'stopping' and active['id'] not in stopping and store.run_live_input(active):
            delivery = 'pending-live'
        out.append(dict(s, delivery=delivery, run=run, method=method, reason=reason))
    return out


def get(conn, card_id):
    c = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    if c is None:
        return None
    out = dict(c)
    out['panel'] = json.loads(out['panel']) if out['panel'] else None
    out['deny_tools'] = json.loads(out['deny_tools'])
    out['executor_wait'] = json.loads(out['executor_wait']) if out['executor_wait'] else None
    if out['recheck_every'] is None:
        out['recheck_policy'] = None
        out['recheck_state'] = None
    else:
        out['recheck_policy'] = {'every': out['recheck_every'], 'until': out['recheck_until'], 'max': out['recheck_max']}
        out['recheck_state'] = {'due': out['recheck_due'], 'attempts': out['recheck_attempts']}
    out['blocked_by'] = store.blockers(conn, card_id)
    out['parent'] = store.parent(conn, card_id)
    children = store.children(conn, card_id)
    out['children'] = [{k: ch[k] for k in ('id', 'state', 'optional')} for ch in children]
    out['progress'] = progress(children) if c['kind'] == 'epic' else None
    out['block_reasons'] = block_reasons(c, out['blocked_by'], out['children'])
    out['schedule'], out['instances'], out['series'] = None, None, None
    if c['kind'] == 'recurring':
        instances = store.instances(conn, card_id)
        out['schedule'] = {'every': c['series_every'], 'max_runs': c['series_max'], 'until': c['series_until'],
                           'overlap': c['series_overlap'], 'start': c['not_before'],
                           'stopped': c['series_stopped'] is not None, 'stopped_at': c['series_stopped'],
                           'next_start': period_start(c, c['series_next']) if series_open(c, len(instances)) else None,
                           'created': len(instances), 'skipped': c['series_skipped']}
        if c['series_cron'] is not None:
            out['schedule'].update(cron=c['series_cron'], tz=c['series_tz'], misfire=c['series_misfire'])
            out['schedule']['start_window'] = c['series_start_window']
        out['instances'] = instances
    series = conn.execute("SELECT target_id FROM links WHERE card_id=? AND kind='series'", (card_id,)).fetchone()
    if series is not None:
        out['series'] = {'id': series['target_id'], 'period': c['series_period']}
    out['runs'] = [dict(r) for r in conn.execute('SELECT * FROM runs WHERE card_id=? ORDER BY id', (card_id,))]
    out['gates'] = [dict(r) for r in conn.execute('SELECT * FROM gates WHERE card_id=? ORDER BY id', (card_id,))]
    out['events'] = [dict(r, data=json.loads(r['data']) if r['data'] else None)
                     for r in conn.execute('SELECT * FROM events WHERE card_id=? ORDER BY id', (card_id,))]
    out['outbox'] = [dict(r) for r in conn.execute('SELECT * FROM outbox WHERE card_id=?', (card_id,))]
    out['steers'] = steer_deliveries(conn, card_id)
    return out


def listing(conn, states=None, limit=200):
    q = """SELECT c.id,c.kind,c.card_class,c.owner_visible,c.state,c.wait_reason,c.step,c.review,c.round,c.title,c.created_by,c.source,
                  c.assignee,c.executor,c.on_unavailable,c.model,c.effort,c.panel,c.created_at,c.updated_at,l.target_id AS parent,
                  ls.target_id AS series,c.series_period
           FROM cards c LEFT JOIN links l ON l.card_id=c.id AND l.kind='parent'
           LEFT JOIN links ls ON ls.card_id=c.id AND ls.kind='series'"""
    args = []
    if states:
        q += f" WHERE c.state IN ({','.join('?' * len(states))})"
        args = list(states)
    else:
        q += " WHERE c.state != 'cancelled'"
    q += ' ORDER BY c.created_at DESC LIMIT ?'
    return [dict(r, panel=json.loads(r['panel']) if r['panel'] else None) for r in conn.execute(q, args + [limit])]


def wait(conn, card_id, timeout, poll=0.25):
    """Block until the card is final (done, or blocked other than waiting) or timeout. Returns the card dict."""
    end = time.time() + timeout
    while True:
        c = conn.execute('SELECT state, wait_reason FROM cards WHERE id=?', (card_id,)).fetchone()
        if c is None or store.is_final(c) or time.time() >= end:
            return get(conn, card_id)
        time.sleep(poll)
