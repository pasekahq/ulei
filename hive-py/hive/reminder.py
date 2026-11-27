"""Reminders (ADR-0100): a card with reminder text publishes `reminder_due` at not_before and runs no head. The secretary
claims it, then sends or withdraws it; unclaimed or silent past its timeout, the hive sends the original text itself
through `ulei-tg notify`. A reminder is delivered only with a Telegram receipt."""
import contextlib
import fcntl
import json
import subprocess
from pathlib import Path

from . import api, store

DEFAULTS = {'unclaimed_s': 180, 'claim_silence_s': 300, 'retry_s': 60, 'max_attempts': 20, 'notify_timeout_s': 120}
DUE_NOTE = ('A reminder is due: tell the owner `text`. First claim it with `hive.py reminder claim CARD`, then send it as '
            'is (`hive.py reminder send CARD`) or edited (`--text TEXT`), or withdraw it with `hive.py reminder withdraw '
            'CARD --reason TEXT`. Unclaimed after `unclaimed_s`, or claimed and silent for `claim_silence_s`, the hive '
            'sends `text` to the owner itself.')
FALLBACK_NOTE = ('The hive sent this reminder to the owner directly, word for word: nobody claimed it in time, or the '
                 'claim went silent. The owner has it; do not send it again.')


class SendFailed(Exception):
    """`ulei-tg notify` sent nothing provable: no receipt."""


def config(table):
    """The [reminders] table with DEFAULTS filled in; refused without the paths tg_binary and tg_config or with a timing
    that is not a positive number (max_attempts: a positive integer)."""
    if not isinstance(table, dict):
        raise ValueError(f'config: reminders must be a table, got {table!r}')
    out = {**DEFAULTS, **table}
    for key in ('tg_binary', 'tg_config'):
        if key not in out:
            raise store.missing(f'reminders.{key}')
        if not isinstance(out[key], str) or not out[key]:
            raise ValueError(f'config: reminders.{key} must be a path, got {out[key]!r}')
    for key in DEFAULTS:
        value = out[key]
        kinds, what = (int, 'integer') if key == 'max_attempts' else ((int, float), 'number')
        if isinstance(value, bool) or not isinstance(value, kinds) or value <= 0:
            raise ValueError(f'config: reminders.{key} must be a positive {what}, got {value!r}')
    return out


@contextlib.contextmanager
def dispatch_lock(d):
    """The dispatcher's lock file, held exclusively: a verb and a tick never act on one reminder at once."""
    with Path(d.cfg['db']).with_name('dispatcher.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield


def queue(d, c, kind, payload, now):
    """Inside a tx: the outbox row `hive:<card>:<kind>`."""
    payload = {'source': 'hive', 'card_id': c['id'], 'title': c['title'], **payload, 'next_actor': 'secretary',
               'how_to_read': f"{d.cfg['card_show_command']} {c['id']}"}
    d.conn.execute('INSERT OR IGNORE INTO outbox(event_id,card_id,kind,payload,source_ts,created_at) VALUES(?,?,?,?,?,?)',
                   (f"hive:{c['id']}:{kind}", c['id'], kind, json.dumps(payload, ensure_ascii=False), now, now))


def make_due(d, c, kind='reminder_due', fields=None, **card):
    """Reminder card `c`, created: its row in state due, one reminder_due event, the card waiting in blocked (with the
    columns `card`) and one outbox event of `kind`, in one transaction. The payload holds the text, due_at, the
    timeouts and `fields`, by default the card's series, period and DUE_NOTE. 1 when this call made it due."""
    now, rc = d.clock(), d.cfg['reminders']
    event_id = f"hive:{c['id']}:{kind}"
    with store.tx(d.conn):
        if not store.cas_card(d.conn, c, 'blocked', wait_reason=api.WAIT_REMINDER, **card):
            return 0
        d.conn.execute("INSERT INTO reminders(card_id,state,due_at,updated_at) VALUES(?,'due',?,?)", (c['id'], now, now))
        if fields is None:
            series = d.conn.execute("SELECT target_id FROM links WHERE card_id=? AND kind='series'",
                                    (c['id'],)).fetchone()
            fields = {'series': series['target_id'] if series else None, 'period': c['series_period'], 'note': DUE_NOTE}
        queue(d, c, kind, {'text': c['reminder'], 'due_at': now, 'unclaimed_s': rc['unclaimed_s'],
                           'claim_silence_s': rc['claim_silence_s'], **fields}, now)
        store.event(d.conn, c['id'], 'reminder_due', {'event_id': event_id}, at=now)
    return 1


def fallback(d, actor='dispatcher', created_by=None):
    """Send every open reminder past its deadline (unclaimed_s after due_at, or claim_silence_s after claimed_at) and
    past its retry_at, as `actor`; with `created_by`, only the reminders that actor created. Returns the number of
    sends tried."""
    now, rc = d.clock(), d.cfg['reminders']
    n = 0
    for r in d.conn.execute("""SELECT r.* FROM reminders r JOIN cards c ON c.id=r.card_id
                               WHERE r.state IN ('due', 'claimed') AND c.state='blocked' AND c.wait_reason=?
                               AND (? IS NULL OR c.created_by=?)""",
                            (api.WAIT_REMINDER, created_by, created_by)).fetchall():
        claimed = r['state'] == 'claimed'
        deadline = r['claimed_at'] + rc['claim_silence_s'] if claimed else r['due_at'] + rc['unclaimed_s']
        if now < deadline or r['retry_at'] is not None and now < r['retry_at']:
            continue
        n += 1
        c = api.card_row(d.conn, r['card_id'])
        try:
            send_text(d, c, c['reminder'], actor, why='claim_silent' if claimed else 'unclaimed')
        except SendFailed:
            continue  # recorded as a failed attempt; retried after retry_s
    return n


def notify(rc, text):
    """Run `ulei-tg notify --config` with `text` on stdin: the receipt of exit 0; raises SendFailed on any other end."""
    argv = [rc['tg_binary'], 'notify', '--config', rc['tg_config']]
    try:
        p = subprocess.run(argv, input=text.encode(), capture_output=True, timeout=rc['notify_timeout_s'])
    except subprocess.TimeoutExpired as e:
        raise SendFailed(f"ulei-tg notify: no answer within {rc['notify_timeout_s']} s") from e
    except OSError as e:
        raise SendFailed(f'ulei-tg notify: {e}') from e
    if p.returncode != 0:
        lines = p.stderr.decode(errors='replace').strip().splitlines()
        raise SendFailed(f'ulei-tg notify: exit {p.returncode}' + (f': {lines[-1][:300]}' if lines else ''))
    try:
        receipt = json.loads(p.stdout)
    except ValueError:
        receipt = None
    chats = receipt.get('chats') if isinstance(receipt, dict) else None
    if not chats or not isinstance(chats, list) or not all(
            isinstance(ch, dict) and type(ch.get('chat_id')) is int and ch.get('message_ids')
            and isinstance(ch['message_ids'], list) and all(type(m) is int for m in ch['message_ids']) for ch in chats):
        raise SendFailed('ulei-tg notify: exit 0 without a receipt')
    return receipt


def send_text(d, c, text, actor, why=None):
    """Send `text` of open reminder card `c` and record the result: delivered with its receipt (and, for the fallback
    `why`, a reminder_fallback event), or one failed attempt, the max_attempts-th of which fails the reminder and blocks
    the card. Returns the receipt; raises SendFailed."""
    rc = d.cfg['reminders']
    try:
        receipt = notify(rc, text)
    except SendFailed as e:
        now = d.clock()
        with store.tx(d.conn):
            r = d.conn.execute('SELECT * FROM reminders WHERE card_id=?', (c['id'],)).fetchone()
            attempts = r['attempts'] + 1
            failed = attempts >= rc['max_attempts']
            d.conn.execute('UPDATE reminders SET state=?, attempts=?, error=?, retry_at=?, updated_at=? WHERE card_id=?',
                           ('failed' if failed else r['state'], attempts, str(e), now + rc['retry_s'], now, c['id']))
            store.event(d.conn, c['id'], 'reminder_send_failed', {'by': actor, 'attempt': attempts, 'error': str(e)},
                        actor=actor, at=now)
            if failed:
                d.finish(api.card_row(d.conn, c['id']), 'blocked',
                         f'reminder not delivered after {attempts} attempts: {e}', wait_reason=None)
        raise
    now = d.clock()
    with store.tx(d.conn):
        r = d.conn.execute('SELECT * FROM reminders WHERE card_id=?', (c['id'],)).fetchone()
        d.conn.execute("""UPDATE reminders SET state='delivered', sent_by=?, sent_text=?, receipt=?, sent_at=?, error=NULL,
                          retry_at=NULL, updated_at=? WHERE card_id=?""",
                       (actor, text, json.dumps(receipt), now, now, c['id']))
        card = api.card_row(d.conn, c['id'])
        if not store.cas_card(d.conn, card, 'done', wait_reason=None, finished_at=now, result=text):
            raise RuntimeError(f"reminder {c['id']} changed under the dispatcher lock")
        store.event(d.conn, c['id'], 'reminder_delivered', {'by': actor, 'receipt': receipt,
                                                            'edited': text != c['reminder'], 'fallback': why},
                    actor=actor, at=now)
        if why is not None:
            queue(d, c, 'reminder_fallback', {'text': text, 'why': why, 'due_at': r['due_at'],
                                              'claimed_at': r['claimed_at'], 'sent_at': now, 'receipt': receipt,
                                              'note': FALLBACK_NOTE}, now)
    return receipt


def open_reminder(d, card_id, actor, states):
    """Inside a tx: (card, row) of a reminder in one of `states` that still waits as a reminder; refuses (CardError) an
    actor other than secretary or owner_actor, a card that is not a reminder, not yet due, or in another state."""
    if actor not in ('secretary', d.cfg['owner_actor']):
        raise api.CardError(f"actor must be secretary or {d.cfg['owner_actor']}, got {actor!r}")
    c = api.card_row(d.conn, card_id)
    if c['reminder'] is None:
        raise api.CardError(f'card {card_id} is not a reminder')
    r = d.conn.execute('SELECT * FROM reminders WHERE card_id=?', (card_id,)).fetchone()
    if r is None:
        raise api.CardError(f"reminder {card_id} is not due yet (card {c['state']})")
    if r['state'] not in states:
        raise api.CardError(f"reminder {card_id} is {r['state']}; this needs {' or '.join(states)}")
    if c['state'] != 'blocked' or c['wait_reason'] != api.WAIT_REMINDER:
        raise api.CardError(f"reminder {card_id} no longer waits: card {c['state']}")
    return c, r


def claim(d, card_id, actor):
    """A due reminder becomes claimed by `actor`, which stops its unclaimed fallback."""
    with dispatch_lock(d), store.tx(d.conn):
        open_reminder(d, card_id, actor, ('due',))
        now = d.clock()
        d.conn.execute("UPDATE reminders SET state='claimed', claimed_at=?, claimed_by=?, updated_at=? WHERE card_id=?",
                       (now, actor, now, card_id))
        store.event(d.conn, card_id, 'reminder_claimed', {'by': actor}, actor=actor, at=now)


def send(d, card_id, text, actor):
    """Send a claimed reminder: `text`, or its original text when None. Returns the receipt; raises SendFailed after
    recording a failed attempt."""
    if text is not None:
        api.check_reminder_text(text)
    with dispatch_lock(d):
        with store.tx(d.conn):
            c, _ = open_reminder(d, card_id, actor, ('claimed',))
        return send_text(d, c, c['reminder'] if text is None else text, actor)


def withdraw(d, card_id, reason, actor):
    """A claimed reminder is withdrawn with `reason` and its card cancelled; nothing is sent."""
    if not reason.strip():
        raise api.CardError('a reason is required to withdraw a reminder')
    with dispatch_lock(d), store.tx(d.conn):
        c, _ = open_reminder(d, card_id, actor, ('claimed',))
        now = d.clock()
        d.conn.execute("UPDATE reminders SET state='withdrawn', reason=?, updated_at=? WHERE card_id=?",
                       (reason, now, card_id))
        if not store.cas_card(d.conn, c, 'cancelled', wait_reason=None, finished_at=now, result=reason):
            raise RuntimeError(f'reminder {card_id} changed under the dispatcher lock')
        store.event(d.conn, card_id, 'reminder_withdrawn', {'by': actor, 'reason': reason}, actor=actor, at=now)
