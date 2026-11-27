"""The stall watchdog (ADR-0101): a oneshot outside the dispatcher that a timer runs every 5 minutes. It reads the
store, qb's weekly limits and the two outside sources [watchdog] names, calls no model and is quiet while the hive is
healthy. Each condition that holds and is past its cooldown becomes one alert: a reminder card of the actor `watchdog`,
due at once as a `watchdog_alert` event. An alert nobody claims or whose claim falls silent is sent as it is through
`ulei-tg notify` by this run's fallback or by the dispatcher's, as a reminder is (hive/reminder.py)."""
import contextlib
import fcntl
import json
import re
import subprocess
import time
from pathlib import Path

from . import api, dispatcher, heads, reminder, store
from .watch import HOLD_S

ACTOR = 'watchdog'
# The conditions in the order a run checks them; a wait_nudge_repeat alert is made once per card and pid.
CONDITIONS = ('queue_stalled', 'vendor_cooldown', 'qb_limit', 'series_superseded', 'escalation_unread',
              'bridge_token_refused', 'wait_nudge_repeat')
ONCE = 'wait_nudge_repeat'
COOLDOWNS = {'queue_stalled': 3600, 'vendor_cooldown': 3600, 'qb_limit': 21600, 'series_superseded': 86400,
             'escalation_unread': 86400, 'bridge_token_refused': 3600}
DEFAULTS = {'stall_s': 900, 'qb_usage_pct': 90, 'superseded_runs': 3, 'escalation_age_s': 1800,
            'bridge_pattern': 'token refresh refused', 'bridge_window_s': 600, 'journal_command': ['journalctl', '--user'],
            'nudge_repeats': 3, 'command_timeout_s': 30, 'lock_wait_s': 150}
# Without these a condition is not checked: qb_providers (all providers qb reports), escalations_command
# (escalation_unread) and bridge_unit (bridge_token_refused).
OPTIONAL = ('qb_providers', 'escalations_command', 'bridge_unit')
NUMBERS = ('stall_s', 'escalation_age_s', 'bridge_window_s', 'command_timeout_s', 'lock_wait_s')
COUNTS = ('superseded_runs', 'nudge_repeats')
ALERT_NOTE = ('A hive watchdog alert: the hive looks stalled, and `text` tells the owner how (`condition` and `facts` '
              'say the same as data). First claim it with `hive.py reminder claim CARD`, then send it as is (`hive.py '
              'reminder send CARD`) or edited (`--text TEXT`), or withdraw it with `hive.py reminder withdraw CARD '
              '--reason TEXT`. Unclaimed after `unclaimed_s`, or claimed and silent for `claim_silence_s`, the hive '
              'sends `text` to the owner itself.')


class SourceError(Exception):
    """An outside source the config names failed or answered outside its documented shape."""


def check_command(value, key):
    if not isinstance(value, list) or not value or not all(isinstance(v, str) and v for v in value):
        raise ValueError(f'config: watchdog.{key} must be a non-empty list of strings, got {value!r}')


def config(cfg):
    """The [watchdog] table with DEFAULTS and COOLDOWNS filled in; refuses an unknown key or condition, a timing that is
    not a positive number, a count that is not a positive integer, a usage outside (0, 100], a pattern that is no regular
    expression and a command that is not a non-empty list of strings."""
    table = cfg.get('watchdog', {})
    if not isinstance(table, dict):
        raise ValueError(f'config: watchdog must be a table, got {table!r}')
    unknown = set(table) - set(DEFAULTS) - set(OPTIONAL) - {'cooldown_s'}
    if unknown:
        raise ValueError(f"config: unknown key watchdog.{sorted(unknown)[0]}")
    cooldowns = table.get('cooldown_s', {})
    if not isinstance(cooldowns, dict) or set(cooldowns) - set(COOLDOWNS):
        raise ValueError(f'config: watchdog.cooldown_s must be a table of {", ".join(COOLDOWNS)}, got {cooldowns!r}')
    out = {**DEFAULTS, **table, 'cooldown_s': {**COOLDOWNS, **cooldowns}}
    numbers = [(f'watchdog.{k}', out[k], (int, float), 'number') for k in NUMBERS]
    numbers += [(f'watchdog.{k}', out[k], int, 'integer') for k in COUNTS]
    numbers += [(f'watchdog.cooldown_s.{k}', v, (int, float), 'number') for k, v in out['cooldown_s'].items()]
    for key, value, kinds, what in numbers:
        if isinstance(value, bool) or not isinstance(value, kinds) or value <= 0:
            raise ValueError(f'config: {key} must be a positive {what}, got {value!r}')
    usage = out['qb_usage_pct']
    if isinstance(usage, bool) or not isinstance(usage, (int, float)) or not 0 < usage <= 100:
        raise ValueError(f'config: watchdog.qb_usage_pct must be a number in (0, 100], got {usage!r}')
    pattern = out['bridge_pattern']
    try:
        if not isinstance(pattern, str) or not pattern.strip():
            raise re.error('blank')
        re.compile(pattern)
    except re.error as e:
        raise ValueError(f'config: watchdog.bridge_pattern must be a regular expression, got {pattern!r}') from e
    check_command(out['journal_command'], 'journal_command')
    if 'escalations_command' in out:
        check_command(out['escalations_command'], 'escalations_command')
    if 'bridge_unit' in out and (not isinstance(out['bridge_unit'], str) or not out['bridge_unit'].strip()):
        raise ValueError(f"config: watchdog.bridge_unit must be a unit name, got {out['bridge_unit']!r}")
    providers = out.get('qb_providers', [])
    if not isinstance(providers, list) or not all(isinstance(p, str) and p for p in providers):
        raise ValueError(f'config: watchdog.qb_providers must be a list of provider names, got {providers!r}')
    return out


def minutes(seconds):
    return int(seconds // 60)


class Watchdog:
    """One run on `conn`: `clock` (time.time when None) is the time of every decision, `limits_fetcher` qb's limits as
    the dispatcher reads them (heads.weekly_left), `sleep` the wait between tries of the dispatcher's lock."""

    def __init__(self, cfg, conn, limits_fetcher=None, clock=None, sleep=time.sleep):
        self.cfg, self.conn, self.section = cfg, conn, cfg['watchdog']
        self.limits_fetcher, self.clock, self.sleep = limits_fetcher, clock or time.time, sleep
        self.d = dispatcher.Dispatcher(cfg, conn, limits_fetcher=limits_fetcher, clock=self.clock)

    def run(self):
        """The fallback of the open alerts, then every condition, then the publication of the watchdog's unpublished
        events. Returns {'sent', 'lock', 'fired', 'published', 'errors'}; `errors` names each source that failed."""
        with self.dispatch_lock() as held:
            sent = reminder.fallback(self.d, ACTOR, created_by=ACTOR)
        now = self.clock()
        fired, errors = [], []
        for condition in CONDITIONS:
            try:
                found = getattr(self, condition)(now)
            except SourceError as e:
                errors.append(f'{condition}: {e}')
                continue
            for key, facts, text in found:
                card = self.fire(condition, key, facts, text, now)
                if card is not None:
                    fired.append({'condition': condition, 'key': key, 'card': card})
        rows = self.conn.execute("""SELECT o.event_id FROM outbox o JOIN cards c ON c.id=o.card_id
                                    WHERE o.published_at IS NULL AND c.created_by=? ORDER BY o.created_at""",
                                 (ACTOR,)).fetchall()
        published = sum(self.d.publish(r['event_id']) for r in rows)
        return {'sent': sent, 'lock': 'held' if held else 'timeout', 'fired': fired, 'published': published,
                'errors': errors}

    @contextlib.contextmanager
    def dispatch_lock(self):
        """The dispatcher's lock file, tried every second for lock_wait_s, so neither a tick nor a verb sends an alert
        at the same time. Yields whether it is held: past the wait (a dispatcher stuck inside a tick) the fallback runs
        without it and errs on the side of a duplicate send (ADR-0100 section 7)."""
        with Path(self.cfg['db']).with_name('dispatcher.lock').open('a') as lock:
            for attempt in range(int(self.section['lock_wait_s']) + 1):
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    held = True
                    break
                except BlockingIOError:
                    held = False
                    if attempt < int(self.section['lock_wait_s']):
                        self.sleep(1)
            yield held

    def fire(self, condition, key, facts, text, now):
        """One alert for `key` unless one was made within its condition's cooldown (ever, for ONCE): the card, due at
        once as a watchdog_alert, then the mark `watchdog:<key>`. Returns the card id, None when nothing is made. A run
        that dies between the two finds the same card by its idempotency key and only marks it."""
        mark, _ = store.meta_get(self.conn, f'watchdog:{key}')
        if mark is not None and (condition == ONCE or now - mark['at'] < self.section['cooldown_s'][condition]):
            return None
        fired = mark['fired'] + 1 if mark is not None else 1
        cid, _ = api.create(self.conn, title=f'watchdog: {condition}', goal=text, step=1, source=f'watchdog:{condition}',
                            created_by=ACTOR, card_class='system', idem_key=f'watchdog:{key}:{fired}',
                            not_before=now + HOLD_S, reminder=text, deliver='owner-inbox')
        c = api.card_row(self.conn, cid)
        if c['state'] == 'created':
            reminder.make_due(self.d, c, 'watchdog_alert',
                              {'condition': condition, 'key': key, 'facts': facts, 'note': ALERT_NOTE}, not_before=now)
        with store.tx(self.conn):
            store.meta_set(self.conn, f'watchdog:{key}', {'at': now, 'fired': fired, 'card': cid,
                                                          'condition': condition})
        return cid

    def command(self, argv, what):
        """The stdout of `argv`; raises SourceError on a failed start, a timeout or a non-zero exit."""
        timeout = self.section['command_timeout_s']
        try:
            p = subprocess.run(argv, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired as e:
            raise SourceError(f'{what}: no answer within {timeout} s') from e
        except OSError as e:
            raise SourceError(f'{what}: {e}') from e
        if p.returncode != 0:
            lines = p.stderr.decode(errors='replace').strip().splitlines()
            raise SourceError(f'{what}: exit {p.returncode}' + (f': {lines[-1][:300]}' if lines else ''))
        return p.stdout.decode(errors='replace')

    # Each condition: [(key, facts, text)] of what holds now.

    def queue_stalled(self, now):
        """Cards that may launch have waited stall_s or longer, and either the dispatcher has not ticked for stall_s or
        no head runs and none started within stall_s."""
        stall = self.section['stall_s']
        waiting = self.conn.execute("""SELECT * FROM cards WHERE (state='queued' AND (retry_at IS NULL OR retry_at<=?)
                                       AND executor_wait IS NULL) OR (state='created' AND (not_before IS NULL
                                       OR not_before<=?))""", (now, now)).fetchall()
        if not waiting:
            return []
        oldest = min(max(api.waiting_since(self.conn, c, now), c['not_before'] or 0) for c in waiting)
        if now - oldest < stall:
            return []
        _, tick_at = store.meta_get(self.conn, 'dispatcher_tick')
        active = self.conn.execute("SELECT count(*) FROM runs WHERE status IN ('starting', 'running')").fetchone()[0]
        last_start = self.conn.execute('SELECT max(started_at) FROM runs').fetchone()[0]
        tick_age = None if tick_at is None else now - tick_at
        ticking = tick_age is not None and tick_age < stall
        if ticking and (active or last_start is not None and now - last_start < stall):
            return []
        facts = {'cards': len(waiting), 'oldest_wait_s': round(now - oldest), 'active_runs': active,
                 'last_run_start_s': None if last_start is None else round(now - last_start),
                 'tick_age_s': None if tick_age is None else round(tick_age)}
        text = (f'Hive watchdog: {len(waiting)} card(s) wait to run, the oldest for {minutes(now - oldest)} min, '
                + (f'and no head has started for {minutes(now - last_start)} min'
                   if last_start is not None else 'and no head has ever started')
                + (f'; the dispatcher last ticked {minutes(tick_age)} min ago.' if tick_age is not None
                   else '; the dispatcher has never ticked.'))
        return [('queue_stalled', facts, text)]

    def vendor_cooldown(self, now):
        """A vendor in its provider cooldown (the dispatcher's meta `provider_cooldown:<vendor>`)."""
        out = []
        for row in self.conn.execute("SELECT key, value FROM meta WHERE key LIKE 'provider_cooldown:%' ORDER BY key"):
            cooldown = json.loads(row['value'])
            if cooldown['until'] <= now:
                continue
            vendor = row['key'].split(':', 1)[1]
            facts = {'vendor': vendor, 'failures': cooldown['failures'], 'until': cooldown['until']}
            out.append((f'vendor_cooldown:{vendor}', facts,
                        f"Hive watchdog: vendor {vendor} is cooling down after {cooldown['failures']} provider failures; "
                        f"no head of it starts for another {minutes(cooldown['until'] - now)} min."))
        return out

    def qb_limit(self, now):
        """A provider qb reports at or above qb_usage_pct of its weekly limit (heads.weekly_left), limited to
        qb_providers when given."""
        limits = heads.weekly_left(self.conn, self.cfg, self.limits_fetcher, now)
        providers = self.section.get('qb_providers') or sorted(limits)
        out = []
        for provider in providers:
            left = limits.get(provider)
            if left is None or 100 - left < self.section['qb_usage_pct']:
                continue
            facts = {'provider': provider, 'window': 'week', 'left_pct': left, 'usage_pct': round(100 - left, 1),
                     'exhausted': left <= 0}
            out.append((f'qb_limit:{provider}', facts,
                        f'Hive watchdog: {provider} has used its whole weekly limit in qb.' if left <= 0 else
                        f'Hive watchdog: {provider} has used {round(100 - left)}% of its weekly limit in qb.'))
        return out

    def series_superseded(self, now):
        """The last superseded_runs ended instances of an active series were each cancelled as superseded."""
        n = self.section['superseded_runs']
        out = []
        for s in self.conn.execute("SELECT id FROM cards WHERE kind='recurring' AND state='blocked' AND wait_reason=?",
                                   (api.WAIT_SCHEDULE,)).fetchall():
            ended = [i for i in store.instance_states(self.conn, s['id']) if store.is_final(i)][-n:]
            if len(ended) < n or not all(self.conn.execute(
                    "SELECT 1 FROM events WHERE card_id=? AND kind='cancelled' AND actor='superseded'",
                    (i['id'],)).fetchone() for i in ended):
                continue
            facts = {'series': s['id'], 'instances': [i['id'] for i in ended], 'periods': [i['period'] for i in ended]}
            out.append((f"series_superseded:{s['id']}", facts,
                        f"Hive watchdog: the last {n} runs of series {s['id']} were each superseded by the next "
                        'before they ran.'))
        return out

    def escalation_unread(self, now):
        """An escalation of escalations_command unread escalation_age_s or longer. The command prints a JSON list of
        {"id": string, "created_at": unix seconds, "read": boolean}."""
        if 'escalations_command' not in self.section:
            return []
        try:
            items = json.loads(self.command(self.section['escalations_command'], 'escalations_command'))
        except ValueError as e:
            raise SourceError(f'escalations_command: not JSON: {e}') from e
        shape = isinstance(items, list) and all(
            isinstance(i, dict) and isinstance(i.get('id'), str) and i['id'] and isinstance(i.get('read'), bool)
            and not isinstance(i.get('created_at'), bool) and isinstance(i.get('created_at'), (int, float))
            for i in items)
        if not shape:
            raise SourceError('escalations_command: not a list of {"id", "created_at", "read"}')
        out = []
        for i in items:
            age = now - i['created_at']
            if i['read'] or age < self.section['escalation_age_s']:
                continue
            out.append((f"escalation_unread:{i['id']}", {'escalation': i['id'], 'created_at': i['created_at'],
                                                          'age_s': round(age)},
                        f"Hive watchdog: escalation {i['id']} has been unread for {minutes(age)} min."))
        return out

    def bridge_token_refused(self, now):
        """A line of bridge_unit's journal within bridge_window_s that matches bridge_pattern. The journal is
        `journal_command --unit=UNIT --since=@T --output=json --no-pager --quiet`, one JSON object per line."""
        if 'bridge_unit' not in self.section:
            return []
        window = self.section['bridge_window_s']
        argv = [*self.section['journal_command'], f"--unit={self.section['bridge_unit']}",
                f'--since=@{int(now - window)}', '--output=json', '--no-pager', '--quiet']
        pattern = re.compile(self.section['bridge_pattern'])
        times = []
        for line in self.command(argv, 'journal_command').splitlines():
            if not line.strip():
                continue
            try:
                entry = json.loads(line)
                at = int(entry['__REALTIME_TIMESTAMP']) / 1e6
                message = entry['MESSAGE']
            except (ValueError, KeyError, TypeError) as e:
                raise SourceError(f'journal_command: a line is no journal entry: {e!r}') from e
            if isinstance(message, list):  # the journal's JSON of a message that is not valid UTF-8
                message = bytes(message).decode(errors='replace')
            if now - window <= at <= now and isinstance(message, str) and pattern.search(message):
                times.append(at)
        if not times:
            return []
        facts = {'lines': len(times), 'last_at': max(times), 'window_s': window}
        return [('bridge_token_refused', facts,
                 f'Hive watchdog: the MCP bridge refused a token refresh {len(times)} time(s) in the last '
                 f'{minutes(window)} min.')]

    def wait_nudge_repeat(self, now):
        """A running card got nudge_repeats or more wait_nudge events in a row on one command (the same pid)."""
        n = self.section['nudge_repeats']
        out = []
        for c in self.conn.execute("SELECT id FROM cards WHERE state='running' ORDER BY id").fetchall():
            streaks, pid, count = {}, None, 0
            for row in self.conn.execute("SELECT data FROM events WHERE card_id=? AND kind='wait_nudge' ORDER BY id",
                                         (c['id'],)):
                data = json.loads(row['data'])
                count = count + 1 if data['pid'] == pid else 1
                pid = data['pid']
                if count >= n:
                    streaks[pid] = {'card': c['id'], 'pid': pid, 'nudges': count, 'waited_s': data['waited_s']}
            for pid, facts in streaks.items():
                out.append((f"wait_nudge_repeat:{c['id']}:{pid}", facts,
                            f"Hive watchdog: card {c['id']} got {facts['nudges']} wait nudges in a row on one command "
                            f"(pid {pid}), which has run for {minutes(facts['waited_s'])} min."))
        return out
