"""A nudge to a head that has waited on one command for long: the dispatcher's insurance against a head stuck in a
wait (a loop that never ends, a command that hung), whatever the command is.

The dispatcher reads the processes of every running worker's unit (`inspect.unit_view`: the cgroup and /proc, no
other lookup) and takes the command the head runs longest, as `hive.py inspect` shows it under `waiting_on`. When that
command has run `interval_s` or longer, it journals a `wait_nudge` event on the card; the runner hands the event's
text to the head over its live input channel and journals `wait_nudge_live` when the vendor acknowledges it. The
nudge repeats every `interval_s` while the same command (same pid and command line) runs; another command starts the
count again. It never stops, restarts or fails a head, and a head need not quote it in its report: the head decides.
A run without a live input channel gets the journal event only. `interval_s = 0` turns the nudge off."""
import json

from . import inspect, store

DEFAULT_TEXT = ('You have been waiting for a command for {minutes} minutes: {command}\n'
                'Are you sure it has not hung and all is well? Check its state and its result; stop it if needed '
                'and go on with the card.')
DEFAULTS = {'interval_s': 1200, 'text': DEFAULT_TEXT}
PLACEHOLDERS = {'minutes': 0, 'command': ''}
COMMAND_CHARS = 300   # the command line in a nudge, redacted by the probe
CHECK_S = 60          # the dispatcher probes the heads' processes at most this often
HELPER_S = 30         # a child started this close to its head process is the vendor CLI's own helper

ACTIVE_WORKERS = """SELECT r.* FROM runs r JOIN cards c ON c.id=r.card_id
                    WHERE c.state='running' AND c.panel IS NULL AND r.role='worker' AND r.round=c.round
                    AND r.status='running' AND r.unit IS NOT NULL AND r.vendor IS NOT c.executor AND r.started_at<=?"""


def config(cfg):
    """The [head_wait_nudge] section with its defaults; refuses a negative or non-numeric interval and a text that is
    blank or names a placeholder other than {minutes} and {command}."""
    section = {**DEFAULTS, **cfg.get('head_wait_nudge', {})}
    interval = section['interval_s']
    if isinstance(interval, bool) or not isinstance(interval, (int, float)) or interval < 0:
        raise ValueError(f'config: head_wait_nudge.interval_s must be a non-negative number, got {interval!r}')
    text = section['text']
    if not isinstance(text, str) or not text.strip():
        raise ValueError('config: head_wait_nudge.text must be a non-blank string')
    try:
        text.format(**PLACEHOLDERS)
    except (KeyError, IndexError, ValueError) as e:
        raise ValueError(f'config: head_wait_nudge.text may name only {{minutes}} and {{command}}: {e!r}') from e
    return section


def longest(probe, run):
    """{pid, cmdline, age_s} of the command the run's head runs longest, None when it runs none. A child the vendor
    CLI started with itself (within HELPER_S of the head process: a local tool server of its session) is no command
    the head waits on."""
    view, procs = inspect.unit_view(probe, run['unit'], run['pid'])
    by_pid = {p['pid']: p for p in procs}

    def own_helper(c):
        head = by_pid.get(by_pid[c['pid']]['ppid']) if c['pid'] in by_pid else None
        return head is not None and head['age_s'] is not None and head['age_s'] - c['age_s'] < HELPER_S
    cmds = [c for c in view['commands'] if c['age_s'] is not None and not own_helper(c)]
    if not cmds:
        return None
    c = max(cmds, key=lambda c: c['age_s'])
    return {'pid': c['pid'], 'cmdline': inspect.cut(c['cmdline'], COMMAND_CHARS), 'age_s': c['age_s']}


def last(conn, run_id, card_id):
    """(data, at) of the run's latest nudge, (None, None) when it has none."""
    row = conn.execute("""SELECT data, at FROM events WHERE card_id=? AND kind='wait_nudge'
                          AND json_extract(data, '$.run')=? ORDER BY id DESC LIMIT 1""", (card_id, run_id)).fetchone()
    return (json.loads(row['data']), row['at']) if row else (None, None)


def nudge(conn, section, run, command, now):
    """Inside a tx: journal the run's next nudge for `command` when one is due; returns the event's data or None."""
    interval = section['interval_s']
    if not interval or command is None or command['age_s'] < interval:
        return None
    prior, at = last(conn, run['id'], run['card_id'])
    same = prior is not None and prior['pid'] == command['pid'] and prior['command'] == command['cmdline']
    if same and now - at < interval:
        return None
    minutes = int(command['age_s'] // 60)
    data = {'run': run['id'], 'round': run['round'], 'pid': command['pid'], 'command': command['cmdline'],
            'waited_s': round(command['age_s'], 1), 'count': prior['count'] + 1 if same else 1,
            'live': store.run_live_input(run),
            'text': section['text'].format(minutes=minutes, command=command['cmdline'])}
    store.event(conn, run['card_id'], 'wait_nudge', data, at=now)
    return data


def after(conn, run_id, card_id, mark=0):
    """[(event id, text)] of the run's nudges journaled after event `mark`, oldest first."""
    rows = conn.execute("""SELECT id, data FROM events WHERE card_id=? AND id>? AND kind='wait_nudge'
                           AND json_extract(data, '$.run')=? ORDER BY id""", (card_id, mark, run_id)).fetchall()
    return [(r['id'], json.loads(r['data'])['text']) for r in rows]
