"""Dispatcher: the ONLY component that moves cards.

tick() = lock → reap runs whose process is gone or past their timeout → advance cards (stop workers that missed a steer, poll external
executors, wake cards whose waited processes ended, handle finished runs, gate, judge, retry/steer round/block) → send admitted external-executor cards and
launch heads into free slots → publish outbox to the owner inbox.
Every transition is a CAS inside BEGIN IMMEDIATE; the outbox row for a final
state is written in the same transaction as the state change.

Lanes are pluggable (LANES): the MVP has one, 'task', limited by `concurrency`.
"""
import fcntl
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import uuid
from pathlib import Path

from . import api, c4g, delivery, judge, liveness, store, heads, resources

REPO = Path(__file__).resolve().parents[1]
EXCERPT = 2500
# A first line that says the work is unfinished is never a result.
PLACEHOLDER_EXIT = 97
PLACEHOLDER_RE = re.compile(
    r'in[\s_-]+progress|placeholder|provisional|will\s+be\s+overwritten|overwritten\s+on\s+(?:finish|completion)'
    r'|\bwaiting\b|\brunning\b|\bзапущен\w*|\b(?:ещё|еще)\s+не\b|\bжду\b|\bжд[её]м\b|заглушк\w*',
    re.I)
_HEAD_FIRST = re.compile(r'''\b(?:head\s+(?:-n\s*1|-1|-n1)|sed\s+-n\s+['"]?1p['"]?)\s+['"]?([^\s'"|;&<>]+)''')
STOP_GRACE_S = 60
# A live head reads steer input only between tool calls; 45 s restarted heads mid-Bash.
LIVE_STEER_GRACE_S = 90
SEND_UNKNOWN = 'send_unknown'
SCRIPT_DEFER_EXIT = 75  # EX_TEMPFAIL: the script asks to be run again later
STUCK_ROUNDS = 2
BREAK_LINES = 20
BREAK_CHARS = 2000
SHOWN_PROCESSES = 3
SHOWN_CMDLINE = 100


def _now():
    return time.time()


class Lane:
    """A queue of card kinds with its own slot limit. Fast lanes plug in here later."""
    def __init__(self, name, slots):
        self.name, self.slots = name, slots


def lanes(cfg):
    return [Lane('task', int(cfg.get('concurrency', 2)))]


class Dispatcher:
    def __init__(self, cfg, conn=None, limits_fetcher=None, launcher=None, clock=_now,
                 cgroup_root=liveness.CGROUP_ROOT, proc_root=liveness.PROC_ROOT):
        self.cfg = cfg
        self.cgroup_root = cgroup_root
        self.proc_root = proc_root
        self.conn = conn or store.connect(cfg['db'], owner_actor=cfg['owner_actor'])
        self.limits_fetcher = limits_fetcher
        self.launcher = launcher or cfg.get('launcher', 'systemd')
        self._adapter = None
        self.clock = clock
        self._host_limit = None
        self._urgent_host_limit = None
        self._floors_ok = False

    def admission_limit(self, ceiling, urgent):
        """Count below which a launch fits one ceiling: urgent_reserve under it for non-urgent, over it for urgent."""
        reserve = self.cfg['admission']['urgent_reserve']
        return ceiling + reserve if urgent else ceiling - reserve

    def cap_bypass(self, urgent, pool_active, pool_ceiling):
        """'cap' when an urgent launch is admitted at a count where a non-urgent one would be refused, else None.
        Called under the claim's write lock, before the new run is inserted."""
        if not urgent:
            return None
        counts = [(pool_active, pool_ceiling)]
        res = self.cfg.get('resources', {})
        if res.get('enabled', False):
            counts.append((self.local_active(), res.get('max_local_runs', 2)))
        return 'cap' if any(active >= self.admission_limit(ceiling, False) for active, ceiling in counts) else None

    def pressure_bypass(self, urgent):
        """'pressure' when an urgent head or script launch passes the closed host gate on its hard floors, else None.
        Valid after resource_room() sampled this tick; at most urgent_reserve such runs are active at once."""
        res = self.cfg.get('resources', {})
        if not urgent or not res.get('enabled', False):
            return None
        active = self.local_active()
        if active < self._host_limit or active >= min(self._urgent_host_limit,
                                                      self.admission_limit(res.get('max_local_runs', 2), True)):
            return None
        held = self.conn.execute('''SELECT count(*) FROM runs r JOIN events e ON e.card_id=r.card_id
            AND e.kind='claimed' AND json_extract(e.data, '$.run')=r.id
            WHERE r.status IN ('starting','running') AND json_extract(e.data, '$.bypass')='pressure'
            ''').fetchone()[0]
        return 'pressure' if held < self.cfg['admission']['urgent_reserve'] else None

    def record_urgent(self, card_id, run_id, bypass):
        """Inside the claim tx: keep the last urgent admission and the gate reasons it bypassed in resource_gate."""
        if not self.cfg.get('resources', {}).get('enabled', False):
            return
        gate, _ = store.meta_get(self.conn, 'resource_gate')
        gate['last_urgent'] = {'card': card_id, 'run': run_id, 'bypass': bypass, 'at': self.clock(),
                               'bypassed': gate['reason'] if bypass == 'pressure' else None}
        store.meta_set(self.conn, 'resource_gate', gate)

    def local_active(self):
        """Local runs and detached gate jobs in the shared max_local_runs pool."""
        active = self.conn.execute('''SELECT count(*) FROM runs r JOIN cards c ON c.id=r.card_id
            WHERE r.status IN ('starting','running') AND r.vendor IS NOT c.executor''').fetchone()[0]
        return active + self.conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0]

    def resource_room(self, urgent=False):
        """One shared pool for heads, scripts AND detached acceptance checks.

        Existing runs drain naturally under pressure. No publishing/reaping or
        external job polling is suspended. Admission is only called outside tx.
        """
        cfg = self.cfg.get('resources', {})
        if not cfg.get('enabled', False):
            return True
        # Prune all expired claims before counting, even when admission is closed.
        self.conn.execute('''DELETE FROM gate_jobs WHERE started_at + 90 + CASE mode WHEN 'judge' THEN ? ELSE COALESCE(
            (SELECT gate_timeout FROM cards WHERE cards.id=gate_jobs.card_id), ?) END < ?''',
            (self.cfg['judge']['timeout_s'], self.cfg.get('gate_timeout_s', 300), self.clock()))
        active = self.local_active()
        if self._host_limit is None:
            prior, _ = store.meta_get(self.conn, 'resource_gate')
            try:
                current = resources.sample(disk=Path(cfg['disk']), cgroups=cfg['psi_cgroups'])
                starting = self.conn.execute('''SELECT count(*) FROM runs r JOIN cards c ON c.id=r.card_id
                    WHERE r.status='starting' AND r.vendor IS NOT c.executor''').fetchone()[0]
                # Claimed runners may not have materialized their RSS yet.
                current['available_mb'] -= starting * cfg.get('per_run_mb', 768)
                slots, urgent_slots, state = resources.evaluate(current, prior, cfg, self.clock(),
                                                                self.cfg['admission']['urgent_floor_mb'])
            except (OSError, ValueError, KeyError) as exc:
                slots = urgent_slots = 0
                state = {'reason': f'host sample unavailable: {exc}',
                         'blocked_until': self.clock() + cfg.get('cooldown_s', 60)}
                if prior and 'last_urgent' in prior:
                    state['last_urgent'] = prior['last_urgent']
            with store.tx(self.conn):
                store.meta_set(self.conn, 'resource_gate', state)
            self._host_limit = active + slots
            self._urgent_host_limit = active + urgent_slots
            self._floors_ok = bool(slots or urgent_slots)  # RAM/disk hard floors hold (soft PSI may be closed)
        return active < min(self.admission_limit(cfg.get('max_local_runs', 2), urgent), self._host_limit)

    # ── entry ──────────────────────────────────────────────────────────────
    def tick(self):
        self._host_limit = None
        lock_path = Path(self.cfg['db']).with_name('dispatcher.lock')
        with lock_path.open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                return {'skipped': 'another dispatcher holds the lock'}
            reaped = self.reap()
            self.resource_room()  # sample every tick, including quiet queues: iowait stays recent
            # Owner-visible checks first: slow series/epic bookkeeping must not delay the owner's answer.
            early = self.schedule_checks(owner_only=True)
            summary = {'reaped': reaped, 'advanced': early + self.advance(), 'launched': self.launch()}
            summary['advanced'] += self.advance()   # a just-finished fast fake/gate may complete in the same tick
            summary['published'] = self.publish()
            with store.tx(self.conn):
                store.meta_set(self.conn, 'dispatcher_tick', summary)
            return summary

    # ── reaping: runs whose process is gone or past their timeout ─────────────
    def reap(self):
        """End every active local run that recorded no result while its process is gone (a 'starting' run after
        spawn_grace_s), and kill and end as a timeout every one that ran heartbeat_stale_s (at least 120 s) past its
        step timeout, even when its runner records an exit while being stopped."""
        now, n = self.clock(), 0
        grace = self.cfg.get('spawn_grace_s', 90)
        margin = max(120, self.cfg.get('heartbeat_stale_s', 120))
        for r in self.conn.execute("""SELECT r.* FROM runs r JOIN cards c ON c.id=r.card_id
                                      WHERE r.status IN ('starting','running') AND r.vendor IS NOT c.executor""").fetchall():
            card = self.conn.execute('SELECT script, script_timeout FROM cards WHERE id=?', (r['card_id'],)).fetchone()
            timeout = card['script_timeout'] if card['script'] else self.cfg['timeouts'].get(r['step'], 3600)
            reason = None
            if r['status'] == 'running' and now - r['started_at'] > timeout + margin:
                self.kill(r)
                reason = f'timeout: running {int(now - r["started_at"])}s > {timeout}s'
            elif r['status'] == 'running' and not self.alive(r):
                reason = 'crashed: process gone without a result'
            elif r['status'] == 'starting' and now - r['started_at'] > grace and not self.alive(r):
                reason = 'crashed: head never started'
            if reason:
                log = Path(r['run_dir']) / 'head.log'
                provider_failure = ('before_first_token' if not card['script'] and r['role'] == 'worker' and
                                    reason.startswith('crashed') and not self.stopped_for_steer(r) and
                                    (not log.is_file() or log.stat().st_size == 0) else None)
                timed_out = reason.startswith('timeout')
                # A runner that catches the stop's SIGTERM records an exit first; the timeout still overrides it.
                guard, guard_args = ('handled=0', ()) if timed_out else ('status=?', (r['status'],))
                with store.tx(self.conn):
                    cur = self.conn.execute(
                        f"UPDATE runs SET status=?, ended_at=?, duration_s=?, outcome=?, error=?, provider_failure=? WHERE id=? AND {guard}",
                        ('timeout' if timed_out else 'crashed', now, round(now - r['started_at'], 1),
                         reason.split(':')[0], reason, provider_failure, r['id'], *guard_args))
                    if cur.rowcount:
                        store.event(self.conn, r['card_id'], 'run_reaped', {'run': r['id'], 'role': r['role'], 'reason': reason})
                        n += 1
        return n

    def alive(self, r):
        return liveness.alive(r, self.cgroup_root)

    def kill(self, r):
        if r['unit']:
            subprocess.run(['systemctl', '--user', 'stop', r['unit']], capture_output=True)
        if r['pid']:
            try:
                os.killpg(r['pid'], signal.SIGTERM)
            except (ProcessLookupError, PermissionError):
                pass

    # ── advancing ──────────────────────────────────────────────────────────
    def advance(self):
        n = 0
        now = self.clock()
        n += self.stop_cancelled_runs()
        n += self.expire_script_windows()
        n += self.advance_series(now)
        n += self.unblock_dependencies()
        n += self.check_epics()
        for c in self.conn.execute("SELECT * FROM cards WHERE state='created'").fetchall():
            if c['not_before'] and c['not_before'] > now:
                continue
            with store.tx(self.conn):
                if store.cas_card(self.conn, c, 'queued'):
                    store.event(self.conn, c['id'], 'queued')
                    n += 1
        n += self.stop_for_steers()
        n += self.poll_external(now)
        n += self.wake_after_processes()
        for r in self.conn.execute(f"SELECT * FROM runs WHERE handled=0 AND status IN ({','.join('?' * len(store.FINISHED_RUN))}) ORDER BY id",
                                   store.FINISHED_RUN).fetchall():
            n += self.handle_run(r)
        n += self.schedule_checks()
        for c in self.conn.execute("""SELECT * FROM cards WHERE state='done' AND recheck_due IS NOT NULL
                                    AND recheck_due<=? AND (recheck_until IS NULL OR ?<recheck_until)
                                    AND (recheck_max IS NULL OR recheck_attempts<recheck_max)""", (now, now)).fetchall():
            n += self.schedule_gate(c, 'recheck')
        return n

    def schedule_checks(self, owner_only=False):
        """Detach the gate of every card in check, or its judge once judge_due and past a judge retry_at."""
        n = 0
        for c in self.conn.execute("SELECT * FROM cards WHERE state='check'").fetchall():
            if owner_only and not self.fast_gate(c):
                continue
            if not self.judge_due(c):
                n += self.schedule_gate(c, 'gate')
            elif c['retry_at'] is None or c['retry_at'] <= self.clock():
                n += self.schedule_gate(c, 'judge')
        return n

    @staticmethod
    def fast_gate(c):
        """Owner-visible acceptance is the last step before the owner hears the answer."""
        return api.owner_card(c) or bool(c['owner_visible'])

    def gate_room(self, c):
        """Admission for one detached acceptance check. An owner-visible check is a short shell command under
        ionice idle; it skips the soft PSI/cooldown gate and the pool count but still honours the RAM/disk
        hard floors."""
        if not self.fast_gate(c):
            return self.resource_room(api.admission_class(c, self.cfg) == 'urgent')
        return self.resource_room(True) or self._floors_ok

    def advance_series(self, now):
        """Per recurring card: free queued instances whose predecessor failed, create at most one instance for the
        latest started period (or skip it on overlap), and finish the series once no instance can follow and the
        last one is final. Period idempotency rests on series_next under the write lock and the instance idem_key."""
        n = 0
        for row in self.conn.execute("SELECT id FROM cards WHERE kind='recurring' AND state='blocked' AND wait_reason=?",
                                     (api.WAIT_SCHEDULE,)).fetchall():
            with store.tx(self.conn):
                s = self.conn.execute('SELECT * FROM cards WHERE id=?', (row['id'],)).fetchone()
                if s['state'] != 'blocked' or s['wait_reason'] != api.WAIT_SCHEDULE:
                    continue
                instances = store.instance_states(self.conn, s['id'])
                n += self.release_after_failed_instance(instances)
                last = instances[-1] if instances else None
                period = None
                if s['series_cron'] is not None:
                    if api.series_open(s, len(instances)):
                        n += self.advance_cron(s, last, now)
                        s = self.conn.execute('SELECT * FROM cards WHERE id=?', (s['id'],)).fetchone()
                        instances = store.instance_states(self.conn, s['id'])
                        last = instances[-1] if instances else None
                else:
                    period = api.due_period(s, now) if api.series_open(s, len(instances)) else None
                if period is not None:
                    n += self.run_period(s, period, last, now)
                    s = self.conn.execute('SELECT * FROM cards WHERE id=?', (s['id'],)).fetchone()
                    instances = store.instance_states(self.conn, s['id'])
                    last = instances[-1] if instances else None
                until = s['series_until']
                closed = not api.series_open(s, len(instances)) or until is not None and now >= until
                if closed and (last is None or store.is_final(last)):
                    reason = 'series stopped' if s['series_stopped'] is not None else 'series complete'
                    if self.finish(s, 'done', f"{reason}: {len(instances)} instance(s), {s['series_skipped']} skipped",
                                   wait_reason=None):
                        n += 1
        return n

    def advance_cron(self, s, last, now):
        """Under the series write lock: consume elapsed cron occurrences exactly once."""
        from .schedule import due_cron, same_local_day
        due = due_cron(s, now)
        if due is None:
            return 0
        period, start, following = due
        first = s['series_next']
        expired = (s['series_until'] is not None and now >= s['series_until'])
        # misfire=skip drops an occurrence only when it can no longer be caught up: a tick that is late because of
        # a transient fault (locked DB, restart) still runs the latest occurrence later on its local day.
        # A start-window series catches up only inside its window (no daytime run of a night job).
        if s['series_start_window'] is not None:
            from .schedule import occurrence_window
            catchable = now < occurrence_window(s['series_start_window'], s['series_tz'], start)[1]
        else:
            catchable = same_local_day(s['series_tz'] or self.cfg['owner_tz'], start, now)
        late = s['series_misfire'] == 'skip' and start <= now - 60 and not catchable
        missed = following <= now or late
        if expired or missed:
            store.event(self.conn, s['id'], 'periods_skipped', {
                'first': first, 'last': period, 'count': period - first + 1, 'reason': 'missed'})
            if missed and not expired:
                self.queue_series_missed(s, period, start, now)
            if not store.cas_card(self.conn, s, 'blocked', series_next=period + 1,
                                  series_due=following, series_skipped=s['series_skipped'] + period - first + 1):
                raise RuntimeError('CAS lost under write lock')
        else:
            self.run_period(s, period, last, now, start=start, following=following)
        return 1

    def queue_series_missed(self, s, period, start, now):
        """Inside a tx. A lost cron occurrence is never silent: one secretary event per lost period."""
        payload = {'source': 'hive', 'card_id': s['id'], 'state': 'missed', 'title': s['title'],
                   'card_class': s['card_class'], 'owner_visible': bool(s['owner_visible']),
                   'period': period, 'scheduled_at': start, 'late_s': int(now - start),
                   'reason': f"series occurrence {period} scheduled at {int(start)} could not be caught up in its day/window "
                             f"(dispatcher was late by {int(now - start)} s) and was skipped",
                   'next_actor': 'secretary',
                   'how_to_read': f"{self.cfg['card_show_command']} {s['id']}",
                   'note': self.cfg['texts']['missed_note']}
        self.conn.execute('INSERT OR IGNORE INTO outbox(event_id,card_id,kind,payload,source_ts,created_at) VALUES(?,?,?,?,?,?)',
                          (f"hive:{s['id']}:missed:{period}", s['id'], 'task_blocked',
                           json.dumps(payload, ensure_ascii=False), now, now))

    def release_after_failed_instance(self, instances):
        """Inside a tx. A queued instance stops waiting for a predecessor instance that ended blocked."""
        n = 0
        failed = {i['id'] for i in instances if i['state'] == 'blocked' and store.is_final(i)}
        for inst in instances:
            if inst['wait_reason'] != api.WAIT_DEPENDENCY:
                continue
            for b in store.blockers(self.conn, inst['id']):
                if b['id'] in failed:
                    self.conn.execute("DELETE FROM links WHERE card_id=? AND kind='blocked_by' AND target_id=?",
                                      (inst['id'], b['id']))
                    store.event(self.conn, inst['id'], 'dependency_removed',
                                {'blocker': b['id'], 'reason': 'previous instance blocked'})
                    n += 1
        return n

    def run_period(self, s, period, last, now, start=None, following=None):
        """Inside a tx. Create the instance of `period` or skip it on overlap; periods missed since series_next are
        counted as skipped in the same single event."""
        first = s['series_next']
        cursor = {'series_due': following} if following is not None else {}
        # Coalesce only work that has never started; active/review/check work stays intact.
        for instance in store.instance_states(self.conn, s['id']):
            if store.is_final(instance):   # a final instance is never waiting: skip its wide row and runs lookup
                continue
            old = api.card_row(self.conn, instance['id'])
            waiting = old['state'] in ('created', 'queued') or (
                old['state'] == 'blocked' and old['wait_reason'] == api.WAIT_DEPENDENCY)
            started = self.conn.execute('SELECT 1 FROM runs WHERE card_id=? LIMIT 1', (old['id'],)).fetchone()
            if waiting and not started:
                self.finish(old, 'cancelled', 'superseded', actor='superseded', wait_reason=None)
        remaining = [i for i in store.instance_states(self.conn, s['id']) if not store.is_final(i)]
        last = remaining[-1] if remaining else None
        overlapping = last is not None and not store.is_final(last)
        if overlapping and s['series_overlap'] == 'skip':
            store.event(self.conn, s['id'], 'periods_skipped', {'first': first, 'last': period, 'count': period - first + 1,
                                                                'reason': 'overlap', 'running': last['id']})
            if not store.cas_card(self.conn, s, 'blocked', series_next=period + 1,
                                  series_skipped=s['series_skipped'] + period - first + 1, **cursor):
                raise RuntimeError('CAS lost under write lock')
            return 1
        if period > first:
            store.event(self.conn, s['id'], 'periods_skipped', {'first': first, 'last': period - 1,
                                                                'count': period - first, 'reason': 'missed'})
        start = api.period_start(s, period) if start is None else start
        fields = {k: s[k] for k in ('kind', 'lane', 'title', 'goal', 'acceptance', 'acceptance_kind', 'judge_criteria',
                                    'deny_tools', 'body', 'step', 'review', 'task_type', 'source', 'created_by',
                                    'priority', 'assignee', 'on_unavailable', 'model', 'effort', 'panel', 'executor',
                                    'script', 'script_timeout', 'deliver', 'gate_timeout', 'owner_visible', 'tag')}
        fields.update(card_class='series_run', kind='task', lane=api.KINDS['task'], idem_key=f"series:{s['id']}:{period}", not_before=start,
                      series_period=period)
        if s['series_start_window'] is not None:
            from .schedule import occurrence_window
            lower, upper = occurrence_window(s['series_start_window'], s['series_tz'], start)
            fields.update(series_start_window=s['series_start_window'], series_tz=s['series_tz'],
                          eligible_from=max(start, lower), eligible_until=upper)
        blocked_by = [last['id']] if overlapping else []
        iid, created = api.insert_card(self.conn, 'h_' + uuid.uuid4().hex[:8], fields, blocked_by, None, False, now)
        if not created:
            raise RuntimeError(f"idem_key of period {period} of series {s['id']} is held by card {iid}")
        self.conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)", (iid, s['id'], now))
        store.event(self.conn, s['id'], 'instance_created', {'card': iid, 'period': period, 'not_before': start,
                                                             'blocked_by': blocked_by})
        if not store.cas_card(self.conn, s, 'blocked', series_next=period + 1,
                              series_skipped=s['series_skipped'] + period - first, **cursor):
            raise RuntimeError('CAS lost under write lock')
        return 1

    def unblock_dependencies(self):
        """Return to created every card waiting on dependencies whose blockers are all done or removed."""
        n = 0
        for c in self.conn.execute("""SELECT * FROM cards c WHERE state='blocked' AND wait_reason=? AND NOT EXISTS (
                                        SELECT 1 FROM links l JOIN cards b ON b.id=l.target_id
                                        WHERE l.card_id=c.id AND l.kind='blocked_by' AND b.state NOT IN ('done','cancelled'))""",
                                   (api.WAIT_DEPENDENCY,)).fetchall():
            with store.tx(self.conn):
                blockers = store.blockers(self.conn, c['id'])
                if all(b['state'] in ('done', 'cancelled') for b in blockers) and store.cas_card(self.conn, c, 'created', wait_reason=None):
                    store.event(self.conn, c['id'], 'dependency_cleared', {'blockers': [b['id'] for b in blockers]})
                    n += 1
        return n

    def check_epics(self):
        """Send to the acceptance gate every epic waiting on children whose required children are all done."""
        n = 0
        for c in self.conn.execute("""SELECT * FROM cards e WHERE kind='epic' AND state='blocked' AND wait_reason=?
                                        AND EXISTS (SELECT 1 FROM links l WHERE l.target_id=e.id AND l.kind='parent' AND l.optional=0)
                                        AND NOT EXISTS (SELECT 1 FROM links l JOIN cards ch ON ch.id=l.card_id
                                          WHERE l.target_id=e.id AND l.kind='parent' AND l.optional=0 AND ch.state NOT IN ('done','cancelled'))""",
                                   (api.WAIT_CHILDREN,)).fetchall():
            with store.tx(self.conn):
                required = [ch['id'] for ch in store.children(self.conn, c['id']) if not ch['optional']]
                if store.cas_card(self.conn, c, 'check', wait_reason=None):
                    store.event(self.conn, c['id'], 'children_done', {'children': required})
                    n += 1
        return n

    def handle_run(self, r):
        limits = heads.weekly_left(self.conn, self.cfg, self.limits_fetcher, self.clock()) if r['provider_failure'] else None
        with store.tx(self.conn):
            c = self.conn.execute('SELECT * FROM cards WHERE id=?', (r['card_id'],)).fetchone()
            cur = self.conn.execute('UPDATE runs SET handled=1 WHERE id=? AND handled=0', (r['id'],))
            if not cur.rowcount:
                return 0
            store.event(self.conn, c['id'], 'run_finished', {
                'run': r['id'], 'role': r['role'], 'status': r['status'], 'exit': r['exit_code'],
                'duration_s': r['duration_s'], 'verdict': r['verdict'], 'model': r['model']})
            expected = 'running' if r['role'] == 'worker' else 'review'
            if c['state'] != expected or c['round'] != r['round']:
                store.event(self.conn, c['id'], 'run_ignored', {'run': r['id'], 'card_state': c['state']})
                return 1
            if r['outcome'] == heads.TAG_REFUSED:
                self.finish(c, 'blocked', f"launcher {r['error']}", wait_reason=api.WAIT_TAG)
                return 1
            if c['panel'] is not None:
                return self.handle_panel_run(c, r)
            if c['script'] is not None:
                if r['status'] == 'exited' and r['exit_code'] == 0:
                    store.cas_card(self.conn, c, 'check')
                elif r['status'] == 'exited' and r['exit_code'] == SCRIPT_DEFER_EXIT:
                    self.defer_script(c, r)
                else:
                    self.finish(c, 'blocked', f"script {r['status']} exit={r['exit_code']}: {r['error'] or r['outcome']}")
                return 1
            if r['role'] == 'worker':
                if r['status'] == 'exited' and self.wait_for_processes(c, r):
                    return 1
                if r['outcome'] == SEND_UNKNOWN:
                    self.finish(c, 'blocked', r['error'])
                    return 1
                if self.stopped_for_steer(r) and self.reopen_for_steer(c, stopped=r):
                    return 1
                # The protocol peer can exit before the next dispatch tick sends a stop.
                live_error = store.live_failure(self.conn, r['id'])
                unseen = [s['id'] for s in store.unseen_steers(self.conn, r['id'])] if live_error else []
                if unseen:
                    store.event(self.conn, c['id'], 'steer_restart', {
                        'run': r['id'], 'steers': unseen, 'reason': live_error})
                    if self.reopen_for_steer(c, stopped=r):
                        return 1
                if r['provider_failure']:
                    self.record_provider_failure(r)
                elif r['status'] == 'exited' and r['exit_code'] == 0:
                    self.conn.execute('DELETE FROM meta WHERE key=?', (f"provider_cooldown:{r['vendor']}",))
                if r['provider_failure'] and c['assignee'] is not None and c['on_unavailable'] == 'wait':
                    if store.cas_card(self.conn, c, 'queued'):
                        store.event(self.conn, c['id'], 'assignee_provider_wait',
                                    {'run': r['id'], 'round': c['round'], 'executor': c['assignee']})
                    return 1
                if r['provider_failure'] and self.failover(c, r, limits):
                    return 1
                if r['provider_failure']:
                    allowed, why = heads.allowed_vendors(self.conn, self.cfg, c['step'], limits, self.clock(),
                                                         tag=c['tag'])
                    vendors = [v for v in self.cfg['ladder'][c['step']]
                               if v in self.cfg['vendors'] and not self.cfg['vendors'][v].get('disabled')]
                    if len(vendors) > 1 and not any(v in allowed for v in vendors) and all(
                            why[v].startswith('provider cooldown') for v in vendors):
                        if store.cas_card(self.conn, c, 'queued'):
                            store.event(self.conn, c['id'], 'provider_outage_wait',
                                        {'run': r['id'], 'round': c['round'], 'vendors': vendors})
                            episode, _ = store.meta_get(self.conn, 'provider_outage_episode')
                            if episode is None:
                                episode = uuid.uuid4().hex
                                store.meta_set(self.conn, 'provider_outage_episode', episode)
                                self.queue_provider_episode(c['id'], episode, 'outage', vendors)
                        return 1
                    self.fail(c, f"worker provider {r['provider_failure']}", backoff=False)
                    return 1
                if r['status'] == 'exited' and r['exit_code'] == 0:
                    if self.repeated_report(c, r):
                        return 1
                    if self.steers_acknowledged(c, r):
                        store.cas_card(self.conn, c, 'check')
                    return 1
                if self.no_result(c, r):
                    self.fail_broken(c, r)
                else:
                    self.fail(c, f"worker {r['status']}: {r['error'] or r['outcome']}")
                return 1
            if r['status'] == 'exited' and r['verdict'] is not None and self.head_round(c):
                if not (r['verdict'] == 'PASS' and self.reopen_for_steer(c)):
                    store.cas_card(self.conn, c, 'check')  # the judge weighs the review
                return 1
            if r['status'] == 'exited' and r['verdict'] == 'PASS':  # an external executor's round: the review decides
                if not self.reopen_for_steer(c):
                    self.finish(c, 'done', f"review PASS by {r['model']}")
                return 1
            if self.no_result(c, r):
                self.fail_broken(c, r)
                return 1
            why = f"review {r['verdict'] or 'no verdict'} by {r['model']}" if r['status'] == 'exited' else f"reviewer {r['status']}"
            self.fail(c, why, feedback=(r['report'] or '')[:4000])
            return 1

    def panel_runs(self, c, round_no):
        """Latest attempt for each member in a persisted panel round."""
        return {r['member']: r for r in self.conn.execute(
            "SELECT * FROM runs WHERE card_id=? AND round=? AND member IS NOT NULL ORDER BY id",
            (c['id'], round_no))}

    def handle_panel_run(self, c, r):
        """Inside a tx. Keep member reports; only a complete synthesis reaches the gate."""
        if r['provider_failure']:
            self.record_provider_failure(r)
        report = api.fresh_report(self.conn, r['id'])
        if r['status'] != 'exited' or r['exit_code'] != 0 or r['provider_failure'] or not report or not report.strip():
            reason = f"panel {r['member']} {r['provider_failure'] or r['error'] or r['outcome'] or r['status']}"
            if r['status'] == 'exited' and r['exit_code'] == 0 and not (report and report.strip()):
                reason = f"panel {r['member']} missing report"
            self.fail(c, reason, retry_state='running', backoff=not r['provider_failure'])
            return 1
        self.conn.execute('DELETE FROM meta WHERE key=?', (f"provider_cooldown:{r['vendor']}",))
        if r['member'] == store.SYNTHESIS:
            missing = [m['label'] for m in json.loads(c['panel'])
                       if not re.search(r'(?<![\w-])' + re.escape(m['label']) + r'(?![\w-])', report)]
            if missing:
                reason = 'panel synthesis omitted members: ' + ', '.join(missing)
                self.conn.execute('UPDATE runs SET error=? WHERE id=?', (reason, r['id']))
                self.fail(c, reason, retry_state='running')
            elif self.repeated_report(c, r):
                pass
            elif store.cas_card(self.conn, c, 'check'):
                store.event(self.conn, c['id'], 'panel_synthesized', {'run': r['id'], 'round': c['round']})
            return 1
        root = Path(c['workdir'])
        (root / r['report_file']).write_text(report)
        store.event(self.conn, c['id'], 'panel_report', {'run': r['id'], 'round': c['round'], 'member': r['member'],
                                                       'file': r['report_file']})
        return 1

    def panel_ready(self, c):
        """Every configured member has a handled, successful report in this round."""
        latest = self.panel_runs(c, c['round'])
        return all(m['label'] in latest and latest[m['label']]['handled'] and
                   self.panel_success(latest[m['label']]) for m in json.loads(c['panel']))

    def panel_success(self, r):
        return (r['status'] == 'exited' and r['exit_code'] == 0 and not r['provider_failure']
                and bool((api.fresh_report(self.conn, r['id']) or '').strip()))

    def record_provider_failure(self, r):
        """Persist the vendor's next eligible time after a handled worker failure."""
        key = f"provider_cooldown:{r['vendor']}"
        prior, _ = store.meta_get(self.conn, key)
        failures = (prior['failures'] if prior else 0) + 1
        cooldown = self.cfg.get('provider_cooldown', {})
        base = cooldown.get('base_s', 60)
        cap = cooldown.get('max_s', 3600)
        delay = min(cap, base * 2 ** (failures - 1))
        store.meta_set(self.conn, key, {'failures': failures, 'until': self.clock() + delay})

    def failover(self, c, r, limits):
        """Queue one alternate vendor attempt in the current worker round."""
        attempts = self.conn.execute("SELECT count(*) FROM runs WHERE card_id=? AND round=? AND role='worker'",
                                     (c['id'], c['round'])).fetchone()[0]
        other = heads.OTHER[r['vendor']]
        allowed, _ = heads.allowed_vendors(self.conn, self.cfg, c['step'], limits, self.clock(), tag=c['tag'])
        if attempts != 1 or other not in allowed or other not in self.cfg['ladder'][c['step']]:
            return False
        head = heads.worker_head(self.cfg, c, other)
        try:
            heads.command(self.cfg, other, head['model'], head['effort'], c['workdir'], r['run_dir'],
                          json.loads(c['deny_tools']))
        except FileNotFoundError:
            return False
        if store.cas_card(self.conn, c, 'queued'):
            store.event(self.conn, c['id'], 'provider_failover', {'run': r['id'], 'round': c['round'],
                                                                  'from': r['vendor'], 'to': other,
                                                                  'reason': r['provider_failure']})
            return True
        return False

    def acceptance(self, c):
        """Run a card acceptance command with the dispatcher gate environment."""
        workdir = c['workdir'] or str(heads.prepare_workspace(self.cfg, c))
        started = self.clock()
        env = {**self.cfg['gate_env'], 'LANG': os.environ.get('LANG', 'C.UTF-8'), 'HIVE_CARD': c['id']}
        # Gates may inspect user services. Keep their bus connection, without
        # inheriting the dispatcher's credentials or profile environment.
        for key in ('XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'):
            if os.environ.get(key):
                env[key] = os.environ[key]
        # A separate session lets the timeout kill all descendants, including background children.
        p = subprocess.Popen(['bash', '-c', c['acceptance']], cwd=workdir, env=env,
                             stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
                             stdin=subprocess.DEVNULL, start_new_session=True)
        try:
            out, _ = p.communicate(timeout=c['gate_timeout'] or self.cfg.get('gate_timeout_s', 300))
            code = p.returncode
            if len(out) > 4000:
                out = '[sha256:' + hashlib.sha256(out.encode()).hexdigest() + ']\n' + out[-4000:]
        except subprocess.TimeoutExpired:
            try:
                os.killpg(p.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            p.communicate()
            code, out = 124, 'gate timeout'
        return code, out, started

    def schedule_gate(self, c, mode):
        """Persist a claim, then detach a gate process; never wait under dispatcher.lock.
        Orphaned claims expire after the command deadline and interpreter startup grace.
        Results still use the original card version for CAS, so steer/cancel wins.
        """
        if not self.gate_room(c) or not self.claim(c, mode):
            return 0
        try:
            subprocess.Popen(resources.background([sys.executable, '-m', 'hive.gate', self.cfg['_path'], self.cfg['db'],
                              c['id'], str(c['version']), mode]), cwd=str(REPO),
                             env=dict(os.environ, PYTHONPATH=str(REPO)), start_new_session=True,
                             stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except OSError:
            self.conn.execute('DELETE FROM gate_jobs WHERE card_id=? AND version=? AND mode=?',
                              (c['id'], c['version'], mode))
            raise
        return 1

    def claim(self, c, mode):
        """Persist the gate_jobs claim of `mode` for this card version after dropping the card's expired ones;
        False while a claim is held."""
        with store.tx(self.conn):
            self.conn.execute("""DELETE FROM gate_jobs WHERE card_id=? AND started_at + 90
                                 + CASE mode WHEN 'judge' THEN ? ELSE ? END < ?""",
                              (c['id'], self.cfg['judge']['timeout_s'], c['gate_timeout'] or self.cfg.get('gate_timeout_s', 300),
                               self.clock()))
            return bool(self.conn.execute('INSERT OR IGNORE INTO gate_jobs VALUES(?,?,?,?)',
                                          (c['id'], c['version'], mode, self.clock())).rowcount)

    def loop_stop(self, c, reason):
        """A repeated outcome needs intervention, not another identical worker round."""
        store.event(self.conn, c['id'], 'loop_stopped', {'round': c['round'], 'reason': reason})
        self.finish(c, 'blocked', reason)


    @staticmethod
    def scheduled_wait(c):
        """Unchanged results are expected while waiting for an external event."""
        return (c['recheck_every'] is not None or c['kind'] == 'recurring'
                or c['card_class'] == 'series_run')

    def repeated_report(self, c, r):
        if self.scheduled_wait(c):
            return False
        prior = self.conn.execute("""SELECT * FROM runs WHERE card_id=? AND role='worker'
                                     AND round<? AND member IS ? ORDER BY id DESC LIMIT 1""",
                                  (c['id'], r['round'], r['member'])).fetchone()
        report = api.fresh_report(self.conn, r['id'])
        # Legacy runs have only a possibly truncated prefix. Never infer full
        # equality from it; new runners atomically save the full-text digest.
        if (prior and report and r['report_sha256']
                and r['report_sha256'] == prior['report_sha256']
                and api.fresh_report(self.conn, prior['id'])):
            # A new owner instruction starts a new attempt, even if its answer is unchanged.
            steer = self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='steer' AND at>?",
                                      (c['id'], prior['started_at'])).fetchone()
            if not steer:
                self.loop_stop(c, 'two consecutive identical worker reports')
                return True
        return False

    @staticmethod
    def placeholder_in(line):
        # A leading label («PLACEHOLDER: fixed ...», «RESULT: ...») names the report, it is not a marker.
        m = PLACEHOLDER_RE.search(re.sub(r'^\s*[A-Z][A-Z0-9_-]*:\s*', '', line or ''))
        return m.group(0) if m else None

    def placeholder_gate(self, c):
        """Why a passed acceptance must still be refused: the first line of the worker's report or of a result
        file the acceptance reads (`head -1 FILE`, FILE under config `results_dir`) says the work is unfinished.
        None if clean."""
        last = self.conn.execute("SELECT report FROM runs WHERE card_id=? AND role='worker' ORDER BY id DESC LIMIT 1",
                                 (c['id'],)).fetchone()
        first = ((last['report'] or '').strip().splitlines() or [''])[0] if last else ''
        found = [('RESULT line of the report', first)]
        workdir = Path(c['workdir']) if c['workdir'] else None
        out_file = re.escape(self.cfg['results_dir'].rstrip('/')) + r'''/[^\s'"|;&<>]+'''
        paths = _HEAD_FIRST.findall(c['acceptance']) + re.findall(out_file, c['acceptance'])
        for raw in dict.fromkeys(paths):
            path = Path(raw)
            if not path.is_absolute() and workdir:
                path = workdir / path
            try:
                if path.is_file():
                    with open(path, 'rb') as f:
                        found.append((f'first line of {raw}', f.readline(2000).decode('utf-8', 'replace').strip()))
            except OSError:
                pass
        for where, line in found:
            marker = self.placeholder_in(line)
            if marker:
                return (f"Gate refused: the {where} reads as an unfinished placeholder (marker «{marker}»): {line[:300]!r}. "
                        "Do the work to the end and write the final result there; never write a result line before the "
                        "final. If something runs long, start it detached, write no result line and end the turn.")
        return None

    def gate(self, c):
        code, out, started = self.acceptance(c)
        refused = self.placeholder_gate(c) if code == 0 else None
        if refused:
            code, out = PLACEHOLDER_EXIT, refused
        passed = code == 0
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if cur['state'] != 'check' or cur['version'] != c['version']:
                return 0
            self.conn.execute('INSERT OR REPLACE INTO gates(card_id,round,command,exit_code,passed,output,at,duration_s) VALUES(?,?,?,?,?,?,?,?)',
                              (c['id'], c['round'], c['acceptance'], code, int(passed), out, self.clock(), round(self.clock() - started, 2)))
            store.event(self.conn, c['id'], 'gate', {'round': c['round'], 'exit': code, 'passed': passed, 'output': out[-500:]})
            if passed and cur['retry_streak']:
                # A passed gate is the forward progress that restarts backoff from base_s.
                if not store.cas_card(self.conn, cur, 'check', retry_streak=0):
                    raise RuntimeError('CAS lost under write lock')
                store.event(self.conn, c['id'], 'backoff_reset', {'round': c['round'], 'streak': cur['retry_streak'],
                                                                  'progress': 'gate passed'})
                cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            feedback = refused or f"Acceptance `{c['acceptance']}` exited {code}:\n{out[-2000:]}"
            if cur['kind'] == 'epic':  # an epic has no worker rounds to retry or steer
                if passed:
                    self.finish(cur, 'done', 'gate passed')
                else:
                    self.finish(cur, 'blocked', f'gate exit {code}', feedback=feedback)
            elif not passed and cur['script'] is not None:
                self.finish(cur, 'blocked', f'gate exit {code}', feedback=feedback)
            elif not passed:
                prior = self.conn.execute('SELECT * FROM gates WHERE card_id=? AND round<? ORDER BY round DESC LIMIT 1',
                                          (c['id'], c['round'])).fetchone()
                steer = prior and self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='steer' AND at>?",
                                                     (c['id'], prior['at'])).fetchone()
                if (not self.scheduled_wait(cur) and prior and not prior['passed']
                        and prior['exit_code'] == code and prior['output'] == out and not steer):
                    self.loop_stop(cur, 'two consecutive identical gate failures')
                    return 1
                self.fail(cur, f'gate exit {code}', feedback=feedback)
            elif self.reopen_for_steer(cur):
                pass
            elif cur['review']:
                store.cas_card(self.conn, cur, 'review')
                store.event(self.conn, c['id'], 'review_requested', {'round': c['round']})
            elif not self.judge_due(cur):
                self.finish(cur, 'done', 'gate passed')
        return 1

    def head_round(self, c):
        """The card's current round ran a head worker: not a script, epic or recurring card, nor an external executor job."""
        if c['script'] is not None or c['kind'] in api.NO_HEADS:
            return False
        worker = self.conn.execute("SELECT vendor FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC LIMIT 1",
                                   (c['id'], c['round'])).fetchone()
        return worker is not None and worker['vendor'] != c['executor']

    def judge_due(self, c):
        """A card in check whose gate passed in this round after a head's worker run, and whose reviewer, if it has
        one, ended this round with a verdict."""
        if c['state'] != 'check' or not self.head_round(c):
            return False
        if not self.conn.execute('SELECT 1 FROM gates WHERE card_id=? AND round=? AND passed=1', (c['id'], c['round'])).fetchone():
            return False
        return not c['review'] or self.review_of(c) is not None

    def review_of(self, c):
        """The handled reviewer run of the card's round that ended with a verdict, else None."""
        return self.conn.execute("""SELECT verdict, report FROM runs WHERE card_id=? AND round=? AND role='reviewer'
                                     AND handled=1 AND verdict IS NOT NULL ORDER BY id DESC LIMIT 1""",
                                 (c['id'], c['round'])).fetchone()

    def judge(self, c):
        """Judge a card judge_due: one C4G turn outside any transaction, then done, a rework round through fail(), a
        retry of the same request at the gateway's time, or a wait in blocked naming the failure class and the tag."""
        if not self.judge_due(c):
            return 0
        report = self.conn.execute("SELECT report FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC LIMIT 1",
                                   (c['id'], c['round'])).fetchone()['report']
        gate = self.conn.execute('SELECT exit_code, output FROM gates WHERE card_id=? AND round=?', (c['id'], c['round'])).fetchone()
        answer = judge.ask(self.cfg, c, report, gate, self.review_of(c) if c['review'] else None)
        found = judge.verdict(answer.text) if isinstance(answer, c4g.Text) else None
        now = self.clock()
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if cur['state'] != 'check' or cur['version'] != c['version']:
                return 0
            data = {'round': c['round'], 'route': self.cfg['judge']['routes'][c['step']]}
            if c['tag'] is not None:
                data['tag'] = c['tag']
            if found is not None:
                store.event(self.conn, c['id'], 'judge', {**data, 'verdict': found[0], 'reason': found[1]})
                if found[0] == 'rework':
                    self.fail(cur, f"judge rework: {found[1].splitlines()[0][:200]}", feedback=found[1])
                elif not self.reopen_for_steer(cur):
                    self.finish(cur, 'done', 'judge done')
            elif isinstance(answer, c4g.Text):
                store.event(self.conn, c['id'], 'judge', {**data, 'failure': 'unparseable', 'stop': answer.stop,
                                                          'answer': answer.text[-500:]})
                self.finish(cur, 'blocked', f'judge unparseable: no verdict in the answer (stop {answer.stop})',
                            wait_reason=api.WAIT_JUDGE)
            elif answer.retryable:
                retry_at = (answer.resets_at if answer.resets_at is not None else
                            now + (answer.retry_after if answer.retry_after is not None else self.cfg['judge']['retry_s']))
                store.event(self.conn, c['id'], 'judge', {**data, 'failure': answer.cls, 'retryable': True,
                                                          'message': answer.message, 'retry_at': retry_at})
                if not store.cas_card(self.conn, cur, 'check', retry_at=retry_at):
                    raise RuntimeError('CAS lost under write lock')
            else:
                store.event(self.conn, c['id'], 'judge', {**data, 'failure': answer.cls, 'retryable': False,
                                                          'message': answer.message})
                tagged = f" for tag {c['tag']}" if c['tag'] is not None else ''
                self.finish(cur, 'blocked', f'judge {answer.cls}{tagged}: {answer.message}', wait_reason=api.WAIT_JUDGE)
        return 1

    def recheck(self, c):
        """Run one due scheduled acceptance check for a completed card."""
        code, out, started = self.acceptance(c)
        passed = code == 0
        attempt = c['recheck_attempts'] + 1
        gate_round = c['round'] + attempt
        next_due = c['recheck_due'] + c['recheck_every']
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if (cur['state'] != 'done' or cur['version'] != c['version'] or cur['recheck_due'] != c['recheck_due']
                    or cur['recheck_until'] is not None and self.clock() >= cur['recheck_until']
                    or cur['recheck_max'] is not None and cur['recheck_attempts'] >= cur['recheck_max']):
                return 0
            self.conn.execute('INSERT OR REPLACE INTO gates(card_id,round,command,exit_code,passed,output,at,duration_s) VALUES(?,?,?,?,?,?,?,?)',
                              (c['id'], gate_round, c['acceptance'], code, int(passed), out, self.clock(),
                               round(self.clock() - started, 2)))
            store.event(self.conn, c['id'], 'gate', {'round': gate_round, 'exit': code, 'passed': passed,
                                                      'output': out[-500:], 'recheck_attempt': attempt})
            if not passed:
                store.event(self.conn, c['id'], 'recheck', {'attempt': attempt, 'exit': code, 'passed': False})
                self.finish(cur, 'blocked', f'recheck exit {code}', recheck_attempts=attempt, recheck_due=None,
                            feedback=f"Acceptance `{c['acceptance']}` exited {code}:\n{out[-2000:]}")
            elif (cur['recheck_max'] is not None and attempt >= cur['recheck_max']
                  or cur['recheck_until'] is not None and next_due >= cur['recheck_until']):
                store.event(self.conn, c['id'], 'recheck', {'attempt': attempt, 'exit': code, 'passed': True})
                self.finish(cur, 'done', 'recheck passed', recheck_attempts=attempt, recheck_due=None)
            elif store.cas_card(self.conn, cur, 'done', recheck_attempts=attempt, recheck_due=next_due):
                store.event(self.conn, c['id'], 'recheck', {'attempt': attempt, 'exit': code, 'passed': True,
                                                            'next_due': next_due})
            else:
                return 0
        return 1

    def defer_script(self, c, r):
        """Inside a tx. Requeue a script that exited SCRIPT_DEFER_EXIT after script_defer_s, spending no failure;
        past script_defer_max deferrals of the card the exit blocks it."""
        deferrals = self.conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='deferred'",
                                      (c['id'],)).fetchone()[0]
        if deferrals >= self.cfg.get('script_defer_max', 6):
            self.finish(c, 'blocked', f"script exit={SCRIPT_DEFER_EXIT} after {deferrals} deferrals")
            return
        tail = (Path(r['run_dir']) / 'stderr.log').read_text(errors='replace')[-500:]
        retry_at = self.clock() + self.cfg.get('script_defer_s', 900)
        if store.cas_card(self.conn, c, 'queued', retry_at=retry_at):
            store.event(self.conn, c['id'], 'deferred', {'run': r['id'], 'round': c['round'], 'retry_at': retry_at,
                                                         'deferrals': deferrals + 1, 'stderr': tail})

    def fail(self, c, reason, feedback=None, retry_state='queued', backoff=True):
        """Inside a tx. Only failures spend max_rounds; launch numbers remain monotonic.
        With [retry_backoff] and backoff, the retry launches no worker for min(max_s, base_s * 2**retry_streak);
        retry_streak stops growing at the cap."""
        max_rounds = self.cfg.get('max_rounds', 2)
        failures = self.conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='retry'",
                                     (c['id'],)).fetchone()[0] + 1
        if failures < max_rounds:
            delay = {}
            if backoff and 'retry_backoff' in self.cfg:
                rb = self.cfg['retry_backoff']
                delay_s = min(rb['max_s'], rb['base_s'] * 2 ** c['retry_streak'])
                delay = {'retry_at': self.clock() + delay_s,
                         'retry_streak': c['retry_streak'] + 1 if delay_s < rb['max_s'] else c['retry_streak']}
            if store.cas_card(self.conn, c, retry_state,
                              feedback=c['feedback'] if retry_state == 'running' else feedback or reason, **delay):
                data = {'round': c['round'], 'reason': reason}
                if delay:
                    data.update(delay_s=delay_s, **delay)
                store.event(self.conn, c['id'], 'retry', data)
        else:
            self.finish(c, 'blocked', reason)

    @staticmethod
    def no_result(c, r):
        """The local run ended without a result: the reaper ended it, or a timeout of the runner or the reaper stopped
        its head, whatever exit the runner recorded."""
        return r['status'] in ('timeout', 'crashed') and r['vendor'] != c['executor']

    def broken_rounds(self, card_id):
        """{round: newest run id}, newest round first, of the card's latest handled runs without a result (no_result),
        back to its newest handled run that recorded a result; a run whose card then waited on the processes it left
        running counts as neither."""
        rounds = {}
        for run in self.conn.execute("""SELECT r.id, r.round, r.status IN ('timeout','crashed') AND r.vendor IS NOT c.executor AS broken,
                                        EXISTS (SELECT 1 FROM events e WHERE e.card_id=r.card_id AND e.kind='process_wait'
                                                AND json_extract(e.data, '$.run')=r.id) AS waited
                                        FROM runs r JOIN cards c ON c.id=r.card_id
                                        WHERE r.card_id=? AND r.handled=1 ORDER BY r.id DESC""", (card_id,)):
            if run['waited']:
                continue
            if not run['broken']:
                break
            rounds.setdefault(run['round'], run['id'])
        return rounds

    def fail_broken(self, c, r):
        """Inside a tx. A round whose run ended without a result fails with the break context as the next worker's
        feedback; the STUCK_ROUNDS-th such round in a row blocks the card waiting as stuck, which notifies the secretary."""
        reason = f"{r['role']} {r['error']}"
        runs = list(self.broken_rounds(c['id']).values())[:STUCK_ROUNDS]
        if len(runs) < STUCK_ROUNDS:
            self.fail(c, reason, feedback=self.break_context(r))
            return
        runs.reverse()
        self.finish(c, 'blocked', f"stuck: {STUCK_ROUNDS} rounds in a row ended without a result "
                                  f"(runs {', '.join(map(str, runs))}); last: {reason}",
                    wait_reason=api.WAIT_STUCK)

    @staticmethod
    def head_output(r):
        """(head.log text, last message) of a head run."""
        log = Path(r['run_dir']) / 'head.log'
        text = log.read_text(errors='replace') if log.is_file() else ''
        return text, heads.parse_usage(r['vendor'], text, r['run_dir'])[3] or ''

    @staticmethod
    def tail_section(name, body):
        """The last BREAK_LINES lines (at most BREAK_CHARS characters) of `body` under a heading naming it."""
        tail = '\n'.join(body.splitlines()[-BREAK_LINES:])[-BREAK_CHARS:]
        return ['', f'Last lines of its {name}:', tail] if tail.strip() else ['', f'Its {name} is empty.']

    def break_context(self, r):
        """What the next worker learns of a run that ended without a result: reason, run id and the bounded last
        lines of its head.log and of its last message."""
        text, message = self.head_output(r)
        return '\n'.join([f"Run {r['id']} ({r['role']}) ended without a result: {r['error']}."]
                         + self.tail_section('head.log', text) + self.tail_section('last message', message))

    def wait_for_processes(self, c, r):
        """Inside a tx. A worker head that exited leaving processes in its unit parks the card waiting on them, with
        their pids and command lines in a process_wait event; False when it left none. Its runner keeps the unit, and
        so the processes, until they end."""
        found = liveness.leftovers(r, self.cgroup_root, self.proc_root)
        if not found:
            return False
        shown = '; '.join(f"{p['pid']} {p['cmdline'][:SHOWN_CMDLINE]}" for p in found[:SHOWN_PROCESSES])
        more = f'; {len(found) - SHOWN_PROCESSES} more' if len(found) > SHOWN_PROCESSES else ''
        if store.cas_card(self.conn, c, 'blocked', wait_reason=api.WAIT_PROCESS,
                          block_reason=f"run {r['id']} left {len(found)} process(es) running: {shown}{more}"):
            store.event(self.conn, c['id'], 'process_wait', {'run': r['id'], 'round': c['round'], 'processes': found})
        return True

    def process_wait(self, card_id):
        """(run row, processes) of the card's latest process_wait event."""
        wait = json.loads(self.conn.execute("SELECT data FROM events WHERE card_id=? AND kind='process_wait' ORDER BY id DESC LIMIT 1",
                                            (card_id,)).fetchone()['data'])
        return self.conn.execute('SELECT * FROM runs WHERE id=?', (wait['run'],)).fetchone(), wait['processes']

    def wake_after_processes(self):
        """Queue every card waiting on processes whose run's unit holds none of them any more. The round spends no
        failure and no backoff; its feedback names the processes, when they were seen ended and the last message."""
        n = 0
        for c in self.conn.execute('SELECT * FROM cards WHERE state=? AND wait_reason=?', ('blocked', api.WAIT_PROCESS)).fetchall():
            r, processes = self.process_wait(c['id'])
            if liveness.leftovers(r, self.cgroup_root, self.proc_root):
                continue
            now = self.clock()
            with store.tx(self.conn):
                if store.cas_card(self.conn, c, 'queued', wait_reason=None, block_reason=None,
                                  feedback=self.process_context(r, processes, now)):
                    store.event(self.conn, c['id'], 'process_ended', {'run': r['id'], 'round': c['round'], 'at': now})
                    n += 1
        return n

    def process_context(self, r, processes, now):
        """What the next worker learns when the processes its predecessor left running have ended."""
        def utc(t):
            return time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime(t))
        lines = [f"Run {r['id']} ({r['role']}) exited at {utc(r['ended_at'])} and left {len(processes)} process(es) "
                 f"running; all of them had ended by {utc(now)}:"]
        lines += [f"- pid {p['pid']}: {p['cmdline']}" for p in processes[:BREAK_LINES]]
        if len(processes) > BREAK_LINES:
            lines.append(f'- and {len(processes) - BREAK_LINES} more')
        return '\n'.join(lines + self.tail_section('last message', self.head_output(r)[1]))

    def reopen_for_steer(self, c, stopped=None):
        """Inside a tx. Unseen steers queue another worker without spending a failure.
        A stopped worker's feedback still applies. Return False when no steer is unseen."""
        if c['panel'] is not None:
            return False
        worker = stopped if stopped is not None else self.conn.execute(
            "SELECT * FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC LIMIT 1",
            (c['id'], c['round'])).fetchone()
        late = [s['id'] for s in store.unseen_steers(self.conn, worker['id'])]
        if not late:
            return False
        if stopped:
            if store.cas_card(self.conn, c, 'queued'):
                store.event(self.conn, c['id'], 'steer_handoff', {'run': stopped['id'], 'round': c['round'],
                                                                   'run_status': stopped['status'], 'steers': late})
        elif store.cas_card(self.conn, c, 'queued', feedback=None):
            store.event(self.conn, c['id'], 'steer_round', {'round': c['round'], 'steers': late})
        return True

    def steers_acknowledged(self, c, r):
        """Inside a tx. Every steer the worker run claimed must be quoted in a report this run's head wrote;
        otherwise the round fails with the same clarifications (retry, or blocked after max_rounds) and False is returned."""
        claim = self.conn.execute("SELECT data FROM events WHERE card_id=? AND kind='claimed' AND json_extract(data, '$.run')=?",
                                  (c['id'], r['id'])).fetchone()
        claimed = sorted(set(json.loads(claim['data']).get('steers', []) + store.live_steers(self.conn, r['id'])))
        if not claimed:
            return True
        texts = {st['id']: st for st in store.steers(self.conn, c['id'])}
        report = api.fresh_report(self.conn, r['id'])
        missing = [i for i in claimed if not api.quotes(report, texts[i]['text'])]
        store.event(self.conn, c['id'], 'steer_check', {'run': r['id'], 'round': r['round'], 'fresh_report': report is not None,
                                                         'missing': missing, 'acknowledged': [i for i in claimed if i not in missing]})
        if not missing:
            return True
        self.fail(c, f"round {r['round']} report does not quote clarification(s) {missing}",
                  feedback='\n\n'.join(['REPORT.md did not quote these owner clarifications. Apply each one and quote it verbatim in REPORT.md:']
                                        + [f"From {texts[i]['by']}:\n{texts[i]['text']}" for i in missing]))
        return False

    def stopped_for_steer(self, r):
        return self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='steer_stop' AND json_extract(data, '$.run')=?",
                                 (r['card_id'], r['id'])).fetchone() is not None

    def stop_for_steers(self):
        """Give live runners time to acknowledge input, then restart unavailable/unresponsive heads.
        Legacy one-shot runners have no input channel. Repeated stops recover lost signals."""
        n = 0
        for r in self.conn.execute("""SELECT r.* FROM runs r JOIN cards c ON c.id=r.card_id WHERE c.state='running'
                                      AND c.panel IS NULL AND r.role='worker' AND r.round=c.round
                                      AND r.status IN ('starting','running')""").fetchall():
            unseen = [s['id'] for s in store.unseen_steers(self.conn, r['id'])]
            if not unseen:
                continue
            live = store.run_live_input(r)
            failed = store.live_failure(self.conn, r['id'])
            sent_at = self.conn.execute('SELECT at FROM events WHERE id=?', (unseen[0],)).fetchone()['at']
            grace = self.cfg.get('live_steer_grace_s', LIVE_STEER_GRACE_S)
            if live and not failed and not self.stopped_for_steer(r) and self.clock() - sent_at < grace:
                continue
            with store.tx(self.conn):
                cur = self.conn.execute('SELECT * FROM runs WHERE id=?', (r['id'],)).fetchone()
                if cur['status'] not in store.ACTIVE_RUN:
                    continue
                # A runner can acknowledge between the outer read and this writer lock.
                unseen = [s['id'] for s in store.unseen_steers(self.conn, r['id'])]
                if not unseen:
                    continue
                # The oldest pending steer may have changed while acquiring the lock.
                sent_at = self.conn.execute('SELECT at FROM events WHERE id=?', (unseen[0],)).fetchone()['at']
                if live and not failed and not self.stopped_for_steer(cur) and self.clock() - sent_at < grace:
                    continue
                if not self.stopped_for_steer(cur):
                    if cur['status'] == 'starting':  # the runner's claim of this run now loses: no head starts
                        self.conn.execute("""UPDATE runs SET status='interrupted', ended_at=?, outcome='interrupted',
                                             error='stopped before start for a steer' WHERE id=? AND status='starting'""",
                                          (self.clock(), r['id']))
                    store.event(self.conn, r['card_id'], 'steer_stop', {'run': r['id'], 'round': r['round'], 'steers': unseen})
                    store.event(self.conn, r['card_id'], 'steer_restart', {
                        'run': r['id'], 'steers': unseen,
                        'reason': failed if failed else
                                  'live input acknowledgement timed out' if live else 'head has no live input channel'})
                    n += 1
            self.kill(cur)
        return n

    def cancel(self, card_id, reason='', actor='secretary', migrate=False, expected_version=None, steward_check=False, keep_card=None, keep_version=None):
        """Cancel under the dispatch lock, then stop and wait for every head.

        Persist first so a crash cannot reopen the card. A retry or next tick
        completes any interrupted stop. Existing instances of a series run on.
        Failed blocked cards can be closed with a non-blank reason; done stays final.
        `migrate` is reserved for an audited legacy cancellation migration.
        """
        owner = self.cfg['owner_actor']
        if actor not in (owner, 'secretary', 'steward'):
            raise api.CardError(f'cancel actor must be {owner}, secretary or steward')
        with Path(self.cfg['db']).with_name('dispatcher.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with store.tx(self.conn):
                c = api.card_row(self.conn, card_id)
                changed = False
                if c['state'] != 'cancelled':
                    if expected_version is not None and c['version'] != expected_version:
                        raise api.CardError('card changed since cancellation plan')
                    if actor == 'steward':
                        from . import steward
                        if expected_version is None or not reason.strip() or not steward.safe(c, self.clock(), owner):
                            raise api.CardError('steward cancellation is protected or missing version/reason')
                        if keep_card is not None:
                            keeper = api.card_row(self.conn, keep_card)
                            if keep_card == card_id or keeper['state'] == 'cancelled' or keeper['version'] != keep_version:
                                raise api.CardError('steward keeper changed since cancellation plan')
                    if steward_check:
                        from . import steward
                        eligible = next((x for x in steward.plan(self.conn, self.cfg, self.clock())['cancel']
                                         if x['id'] == card_id), None)
                        if eligible is None or eligible['reason'] != reason:
                            raise api.CardError('steward reason no longer valid')
                    if store.is_final(c):
                        if c['state'] != 'blocked':
                            raise api.CardError(f"card {card_id} is already final ({c['state']})")
                        if not reason.strip():
                            raise api.CardError('a reason is required to cancel a failed blocked card')
                    fields = dict(wait_reason=None, block_reason=None, executor_wait=None, recheck_due=None)
                    if c['kind'] == 'recurring':
                        fields['series_stopped'] = c['series_stopped'] or self.clock()
                    changed = self.finish(c, 'cancelled', reason, actor=actor, **fields)
            self.stop_cancelled_runs(card_id, wait=True)
            self.stop_waited_processes(card_id)
            self.publish(event_id=f'hive:{card_id}:cancelled')
            return changed

    def stop_cancelled_runs(self, card_id=None, wait=False):
        """The same process-group/unit stop as steer, including reviewers.

        Invalidate unclaimed runners under the writer lock before signalling.
        A running process keeps its slot until it has actually exited.
        """
        rows = self.conn.execute("""SELECT r.* FROM runs r JOIN cards c ON c.id=r.card_id
                                    WHERE c.state='cancelled' AND r.status IN ('starting','running')
                                    AND (? IS NULL OR c.id=?)""", (card_id, card_id)).fetchall()
        for r in rows:
            with store.tx(self.conn):
                # A runner may claim starting->running after the outer query.
                # Use its current PID/unit and never release a live run's slot.
                r = self.conn.execute('SELECT * FROM runs WHERE id=?', (r['id'],)).fetchone()
                if r['status'] not in store.ACTIVE_RUN:
                    continue
                self.conn.execute("""UPDATE runs SET status='interrupted', ended_at=?, outcome='interrupted',
                                     error='cancelled before start' WHERE id=? AND status='starting'""",
                                  (self.clock(), r['id']))
            self.kill(r)
            if wait:
                self.await_exit(r)
            if not self.alive(r):
                with store.tx(self.conn):
                    self.conn.execute("""UPDATE runs SET status='interrupted', ended_at=?, outcome='interrupted',
                                         error='stopped for cancellation' WHERE id=? AND status='running'""",
                                      (self.clock(), r['id']))
        return len(rows)

    def await_exit(self, r):
        """Wait up to 60 s for the process of a stopped run of a cancelled card to be gone."""
        deadline = time.monotonic() + 60
        while self.alive(r):
            if time.monotonic() >= deadline:
                raise api.CardError(f"cancelled {r['card_id']}; run {r['id']} has not exited; retry cancel")
            time.sleep(0.1)

    def stop_waited_processes(self, card_id):
        """Stop the unit, and with it the processes, of the run whose leftovers the cancelled card waited on."""
        last = self.conn.execute("""SELECT kind FROM events WHERE card_id=? AND kind IN ('process_wait','process_ended')
                                     ORDER BY id DESC LIMIT 1""", (card_id,)).fetchone()
        if last is None or last['kind'] != 'process_wait':
            return
        r, _ = self.process_wait(card_id)
        if self.alive(r):  # the unit lives only while its runner does, so its pid is still the runner's
            self.kill(r)
            self.await_exit(r)

    def finish(self, c, state, reason, actor='dispatcher', **fields):
        now = self.clock()
        result = reason
        if c['script'] is not None:
            run = self.conn.execute("SELECT report FROM runs WHERE card_id=? AND role='worker' ORDER BY id DESC LIMIT 1", (c['id'],)).fetchone()
            result = run['report'] if state == 'done' and run is not None else reason
        fields = {'finished_at': now, 'result': result, **fields}
        if state == 'blocked':
            fields['block_reason'] = reason
        if not store.cas_card(self.conn, c, state, **fields):
            return False
        audit = {'reason': reason, 'round': c['round']}
        if state == 'cancelled':
            audit.update(previous_state=c['state'], previous_block_reason=c['block_reason'])
        store.event(self.conn, c['id'], state, audit, actor=actor)
        if actor in ('steward', 'superseded'):
            return True  # Audited in events; routine housekeeping must not notify the owner.
        payload = self.result_payload(c['id'], state, reason)
        self.conn.execute('INSERT OR IGNORE INTO outbox(event_id,card_id,kind,payload,source_ts,created_at) VALUES(?,?,?,?,?,?)',
                          (f"hive:{c['id']}:{state}", c['id'], 'task_blocked' if state == 'blocked' else 'task_result',
                           json.dumps(payload, ensure_ascii=False), now, now))
        return True

    def queue_provider_episode(self, card_id, episode, transition, vendors):
        """Queue one transition for a dispatcher-global provider outage episode."""
        now = self.clock()
        payload = {'source': 'hive', 'episode': episode, 'transition': transition,
                   'vendors': vendors, 'card_id': card_id}
        self.conn.execute('INSERT INTO outbox(event_id,card_id,kind,payload,source_ts,created_at) VALUES(?,?,?,?,?,?)',
                          (f'hive:provider:{episode}:{transition}', card_id, f'provider_{transition}',
                           json.dumps(payload, ensure_ascii=False, sort_keys=True), now, now))

    def result_payload(self, card_id, state, reason):
        c = self.conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
        runs = self.conn.execute('SELECT * FROM runs WHERE card_id=? ORDER BY id', (card_id,)).fetchall()
        workers = [r for r in runs if r['role'] == 'worker']
        last = workers[-1] if workers else None
        gate = self.conn.execute('SELECT * FROM gates WHERE card_id=? ORDER BY id DESC LIMIT 1', (card_id,)).fetchone()
        report = (last['report'] or '') if last else ''
        result_line = report.splitlines()[0] if report else ''
        payload = {
            'result_line': result_line,
            'result': {'news': result_line.startswith('RESULT:') and '[NEWS]' in result_line.split(),
                       'series_stopped': bool(c['series_stopped']) or (
                           result_line.startswith('RESULT:') and '[SERIES-STOPPED]' in result_line.split())},
            'card_class': c['card_class'], 'owner_visible': bool(c['owner_visible']),
            'loop_stopped': bool(self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='loop_stopped'", (card_id,)).fetchone()),
            'source': 'hive', 'card_id': card_id, 'state': state, 'reason': reason, 'title': c['title'],
            'goal': c['goal'], 'acceptance': c['acceptance'], 'created_by': c['created_by'], 'card_source': c['source'],
            'step': c['step'], 'rounds': c['round'], 'review': bool(c['review']),
            'heads': [f"{r['member'] or r['role']}:{r['model']}/{r['effort']}:{r['status']}" + (f":{r['verdict']}" if r['verdict'] else '') for r in runs],
            'duration_s': round(sum(r['duration_s'] or 0 for r in runs), 1),
            'gate': {'exit': gate['exit_code'], 'passed': bool(gate['passed'])} if gate else None,
            'report_excerpt': report if len(report) <= EXCERPT else result_line + '\n' + (last['report_tail'] if last['report_tail'] is not None else report)[-EXCERPT:],
            'steers': [{k: s[k] for k in ('id', 'by', 'delivery', 'run')} for s in api.steer_deliveries(self.conn, card_id)],
            'workdir': c['workdir'], 'next_actor': 'secretary',
            'how_to_read': f"{self.cfg['card_show_command']} {card_id}",
            'note': (self.cfg['texts']['series_news_note'] if c['card_class'] == 'series_run'
                     and result_line.startswith('RESULT:') and '[NEWS]' in result_line.split() else
                     self.cfg['texts']['result_note'])}
        if state == 'blocked' and c['wait_reason'] == api.WAIT_STUCK:
            broken = sorted(self.broken_rounds(card_id).values())[-STUCK_ROUNDS:]
            payload['stuck'] = [{'run': r['id'], 'role': r['role'], 'reason': r['error']} for r in runs if r['id'] in broken]
        return payload

    # ── launching ──────────────────────────────────────────────────────────
    def active_runs(self, lane):
        """Head runs holding a slot of the lane; an external executor run (vendor = card executor) holds none."""
        return self.conn.execute("""SELECT count(*) FROM runs r JOIN cards c ON c.id=r.card_id
                                    WHERE r.status IN ('starting','running') AND c.lane=? AND r.vendor IS NOT c.executor AND c.script IS NULL""",
                                 (lane,)).fetchone()[0]

    def active_scripts(self):
        return self.conn.execute("SELECT count(*) FROM runs WHERE vendor='script' AND status IN ('starting','running')").fetchone()[0]

    def launch_room(self, urgent):
        # A detached gate can finish between two candidates. Once a higher-class
        # candidate was refused, defer lower classes to the next tick as well.
        if getattr(self, '_launch_exhausted', False) and getattr(self, '_launching', False):
            return False
        room = self.resource_room(urgent) or self.pressure_bypass(urgent) is not None
        if not room and getattr(self, '_launching', False):
            self._launch_exhausted = True
        return room

    def launch(self):
        self._launch_exhausted = False
        self._launching = True
        try:
            return self.launch_ordered()
        finally:
            self._launching = False

    def launch_ordered(self):
        launched = []
        # Rank across all lanes and both pools before consuming the shared resource gate.
        for cls in api.ADMISSION_CLASSES:
            if self._launch_exhausted:
                break
            launched.extend(self.launch_class(cls))
        return launched

    def launch_class(self, cls):
        launched = []
        limits = None
        fallback = {}
        now = self.clock()
        urgent = cls == 'urgent'
        for c in self.conn.execute("""SELECT * FROM cards WHERE state='queued' AND executor IS NOT NULL
                                      AND (retry_at IS NULL OR retry_at<=?)
                                      ORDER BY priority DESC, created_at""", (now,)).fetchall():
            if api.admission_class(c, self.cfg) != cls:
                continue
            reason = self.head_round_reason(c) or self.admit_external(c['executor'])
            if reason is None:
                rid = self.send_external(c)
                if rid:
                    launched.append(rid)
            elif c['on_unavailable'] == 'fallback':
                fallback[c['id']] = reason
            else:
                self.wait_for_executor(c, c['executor'], reason)
        for lane in lanes(self.cfg):
            free = self.admission_limit(lane.slots, urgent) - self.active_runs(lane.name)
            if free <= 0:
                continue
            # reviewers first within a class: they finish cards
            cands = self.conn.execute("""SELECT * FROM cards c WHERE lane=? AND script IS NULL AND (
                     (state='review' AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.card_id=c.id AND r.round=c.round AND r.role='reviewer'))
                     OR state='queued' OR (state='running' AND panel IS NOT NULL)) AND (retry_at IS NULL OR retry_at<=?)
                     ORDER BY state='review' DESC, priority DESC, created_at""", (lane.name, now)).fetchall()
            for c in cands:
                if api.admission_class(c, self.cfg) != cls:
                    continue
                if free <= 0:
                    break
                if c['state'] == 'queued' and c['executor'] is not None and c['id'] not in fallback:
                    continue
                if limits is None:
                    limits = heads.weekly_left(self.conn, self.cfg, self.limits_fetcher, self.clock())
                members = json.loads(c['panel']) if c['panel'] else [None]
                for member in members:
                    if free <= 0:
                        break
                    cur = (self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
                           if member is not None else c)
                    if cur['state'] not in ('queued', 'running', 'review'):
                        break
                    rid = self.start_run(cur, limits, member, executor_reason=fallback.get(c['id']))
                    if rid:
                        launched.append(rid)
                        free -= 1
                if c['panel'] and free > 0:
                    cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
                    if cur['state'] == 'running' and self.panel_ready(cur):
                        rid = self.start_run(cur, limits, synthesis=True)
                        if rid:
                            launched.append(rid)
                            free -= 1
        # Scripts have their own bounded pool and never query model quotas or occupy a head slot.
        active = self.active_scripts()
        for c in self.conn.execute("""SELECT * FROM cards WHERE state='queued' AND script IS NOT NULL
                                      AND (retry_at IS NULL OR retry_at<=?) ORDER BY priority DESC,created_at""",
                                   (now,)).fetchall():
            if api.admission_class(c, self.cfg) != cls:
                continue
            if active >= self.admission_limit(self.cfg.get('script_concurrency', 4), urgent):
                break
            rid = self.start_script(c)
            if rid:
                launched.append(rid)
                active += 1
        return launched

    def expire_script_window(self, c, now):
        """Inside a tx. Cancel only an unadmitted occurrence, retaining its series cursor."""
        if c['series_start_window'] is None:
            return False
        if c['eligible_from'] is None or c['eligible_until'] is None:
            reason = 'start window refused: missing occurrence bounds'
        elif now >= c['eligible_until']:
            reason = f"start window expired: admission deadline {c['eligible_until']} ({c['series_tz']})"
        else:
            return False
        return self.finish(c, 'cancelled', reason, wait_reason=None)

    def expire_script_windows(self):
        """Expire waiting occurrences even while resources, dependencies or the script pool are full."""
        n = 0
        for row in self.conn.execute("""SELECT id FROM cards WHERE script IS NOT NULL AND series_start_window IS NOT NULL
            AND (state IN ('created','queued') OR (state='blocked' AND wait_reason=?))""",
                                     (api.WAIT_DEPENDENCY,)).fetchall():
            with store.tx(self.conn):
                cur = api.card_row(self.conn, row['id'])
                if cur['state'] in ('created', 'queued') or (cur['state'] == 'blocked' and cur['wait_reason'] == api.WAIT_DEPENDENCY):
                    n += self.expire_script_window(cur, self.clock())
        return n

    def start_script(self, c):
        if not self.launch_room(api.admission_class(c, self.cfg) == 'urgent'):
            return None
        from .script import command
        workdir = heads.prepare_workspace(self.cfg, c)
        run_dir = workdir / '.hive' / f'script-{uuid.uuid4().hex}'
        run_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        argv = command(c['script'])
        job = {'script': True, 'argv': argv, 'cwd': str(workdir), 'prompt': '',
               'timeout': c['script_timeout'], 'vendor': 'script', 'role': 'worker', 'report_file': 'REPORT.md'}
        job_file = run_dir / 'job.json'
        job_file.write_text(json.dumps(job))
        job_file.chmod(0o600)
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if cur['state'] != 'queued' or cur['version'] != c['version']:
                return None
            admitted_at = self.clock()
            urgent = api.admission_class(cur, self.cfg) == 'urgent'
            bypass = self.pressure_bypass(urgent) or self.cap_bypass(urgent, self.active_scripts(),
                                                                     self.cfg.get('script_concurrency', 4))
            if self.expire_script_window(cur, admitted_at):
                return None
            if cur['eligible_from'] is not None and admitted_at < cur['eligible_from']:
                return None
            rc = self.conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,run_dir)
                                      VALUES(?,?,'worker','script','','',?,'','starting',?,?)""",
                                   (c['id'], c['round'] + 1, c['step'], admitted_at, str(run_dir)))
            rid = rc.lastrowid
            if self.launcher == 'systemd':
                self.conn.execute('UPDATE runs SET unit=? WHERE id=?', (f'hive-run-{rid}', rid))
            if not store.cas_card(self.conn, cur, 'running', round=c['round'] + 1, workdir=str(workdir), retry_at=None):
                raise RuntimeError('CAS lost under write lock')
            store.event(self.conn, c['id'], 'claimed', {'run': rid, 'role': 'worker', 'script': c['script'],
                                                        'admission_class': api.admission_class(cur, self.cfg),
                                                        'bypass': bypass})
            if urgent:
                self.record_urgent(c['id'], rid, bypass)
        try:
            self.spawn(rid, workdir)
        except Exception:
            with store.tx(self.conn):
                self.conn.execute("UPDATE runs SET status='failed_launch', ended_at=?, outcome='failed_launch', error='script launch failed' WHERE id=? AND status='starting'",
                                  (self.clock(), rid))
        return rid

    def start_run(self, c, limits, member=None, synthesis=False, executor_reason=None):
        if not self.launch_room(api.admission_class(c, self.cfg) == 'urgent'):
            return None
        role = 'reviewer' if c['state'] == 'review' else 'worker'
        if c['tag'] is not None:
            vendors = self.head_vendors(c, role, member)
            if not any(heads.tag_capable(self.cfg, v) for v in vendors):
                with store.tx(self.conn):
                    self.finish(c, 'blocked', f"{heads.no_tag_launcher(c['tag'])} among {role} vendors {', '.join(vendors)}",
                                wait_reason=api.WAIT_TAG)
                return None
        fallback_reason = None
        allowed, why = heads.allowed_vendors(self.conn, self.cfg, c['step'], limits, self.clock(), tag=c['tag'])
        if synthesis:
            prior = self.panel_runs(c, c['round']).get(store.SYNTHESIS)
            if prior is not None and not prior['handled']:
                return None
            allowed = [v for v in allowed if v in self.cfg['ladder'][c['step']]]
            for v in heads.VENDORS:
                if v not in self.cfg['ladder'][c['step']]:
                    why[v] = 'no ladder head at this step'
            vendor = heads.pick_vendor(allowed, heads.recent_vendors(self.conn, self.cfg.get('pick_window', 12)),
                                       {v: vc.get('weight', 1.0) for v, vc in self.cfg['vendors'].items()})
        elif member is not None:
            round_no = c['round'] + 1 if c['state'] == 'queued' else c['round']
            prior = self.panel_runs(c, round_no).get(member['label'])
            if prior is not None and (not prior['handled'] or self.panel_success(prior)):
                return None
            vendor = member['vendor']
            if vendor not in allowed:
                self.wait_for_panel_member(c, member, why[vendor])
                return None
        elif role == 'reviewer':
            worker = self.conn.execute("SELECT vendor FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC",
                                       (c['id'], c['round'])).fetchone()
            if worker is not None and worker['vendor'] in heads.OTHER:
                other = heads.OTHER[worker['vendor']]
                vendor = other if other in allowed else None
                # Explicit opt-in for periods when only one head vendor is available.
                if vendor is None and self.cfg.get('review_same_vendor_fallback') and worker['vendor'] in allowed:
                    vendor = worker['vendor']
            else:
                vendor = heads.pick_vendor(allowed, heads.recent_vendors(self.conn, self.cfg.get('pick_window', 12)),
                                           {v: self.cfg['vendors'][v].get('weight', 1.0) for v in heads.VENDORS if v in self.cfg['vendors']})
        else:
            prior = self.conn.execute("SELECT * FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC",
                                      (c['id'], c['round'])).fetchone()
            failover = (prior and prior['provider_failure'] and self.conn.execute(
                "SELECT 1 FROM events WHERE card_id=? AND kind='provider_failover' AND json_extract(data, '$.run')=?",
                (c['id'], prior['id'])).fetchone())
            outage_wait = (prior and self.conn.execute(
                """SELECT 1 FROM events WHERE card_id=? AND kind IN ('provider_outage_wait','assignee_provider_wait')
                   AND json_extract(data, '$.run')=?""", (c['id'], prior['id'])).fetchone())
            if c['assignee'] in allowed:
                vendor = c['assignee']
            elif c['assignee'] is not None and c['on_unavailable'] == 'wait':
                self.wait_for_executor(c, c['assignee'], why[c['assignee']])
                return None
            elif failover:
                other = heads.OTHER[prior['vendor']]
                vendor = other if other in allowed else None
                fallback_reason = f"provider failure {prior['provider_failure']} of run {prior['id']}"
            else:
                known = len(allowed) > 1 and all(isinstance(limits.get(v), (int, float)) for v in allowed)
                preferred = max(allowed, key=limits.__getitem__) if known else None
                if preferred is not None and sum(limits[v] == limits[preferred] for v in allowed) == 1:
                    vendor = preferred
                else:
                    vendor = heads.pick_vendor(allowed, heads.recent_vendors(self.conn, self.cfg.get('pick_window', 12)),
                                               {v: self.cfg['vendors'][v].get('weight', 1.0) for v in heads.VENDORS if v in self.cfg['vendors']})
        if vendor is None:
            key = f"budget_wait:{c['id']}"
            prev, _ = store.meta_get(self.conn, key)
            if prev != why:
                with store.tx(self.conn):
                    store.meta_set(self.conn, key, why)
                    data = {'role': role, 'why': why}
                    if synthesis:
                        data['member'] = store.SYNTHESIS
                    store.event(self.conn, c['id'], 'budget_wait', data)
            return None
        head = ({k: member[k] for k in ('vendor', 'model', 'effort')} if member is not None else
                heads.worker_head(self.cfg, c, vendor) if role == 'worker' and not synthesis else
                heads.head_for(self.cfg, c['step'], vendor))
        root = heads.prepare_workspace(self.cfg, c)
        if member is None:
            if synthesis or role == 'reviewer':
                round_no = c['round']
            else:
                round_no = c['round'] if failover or outage_wait else c['round'] + 1
        workdir = heads.prepare_workspace(self.cfg, c, member, round_no) if member is not None else root
        report_file = f"REPORT-{member['label']}.md" if member is not None else 'REPORT.md' if role == 'worker' else 'REVIEW.md'
        run_dir = workdir / '.hive' / f'r{round_no}-{role}-{vendor}-{uuid.uuid4().hex}'
        run_dir.mkdir(parents=True, exist_ok=True)
        live_input = (role == 'worker' and c['panel'] is None
                      and self.cfg['vendors'][vendor].get('live_input', True))
        try:
            argv = heads.command(self.cfg, vendor, head['model'], head['effort'], workdir, run_dir,
                                json.loads(c['deny_tools']), live=live_input)
        except FileNotFoundError as e:
            if member is not None:
                self.wait_for_panel_member(c, member, str(e))
                return None
            with store.tx(self.conn):
                store.event(self.conn, c['id'], 'launch_error', {'vendor': vendor, 'error': str(e)})
            return None
        now = self.clock()
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if cur['version'] != c['version'] or cur['state'] != c['state']:
                return None
            claim = {'role': role, 'round': round_no, **head}
            slots = next(lane.slots for lane in lanes(self.cfg) if lane.name == cur['lane'])
            lane_active = self.active_runs(cur['lane'])
            urgent = api.admission_class(cur, self.cfg) == 'urgent'
            # Built under the claim's write lock: every steer committed before the claim is in this prompt, and the
            # claim lists those no earlier worker report acknowledged; this run's report must quote them.
            if synthesis:
                latest = self.panel_runs(cur, round_no).get(store.SYNTHESIS)
                if (latest is not None and not latest['handled']) or not self.panel_ready(cur):
                    return None
                allowed, _ = heads.allowed_vendors(self.conn, self.cfg, cur['step'], limits, self.clock(), tag=cur['tag'])
                if lane_active >= self.admission_limit(slots, urgent) or vendor not in allowed:
                    return None
                claim['member'] = store.SYNTHESIS
                feedback = cur['feedback']
                if latest is not None:
                    feedback = (feedback + '\n' if feedback else '') + f"Your previous synthesis failed: {latest['error'] or latest['outcome'] or latest['status']}"
                prompt = heads.synthesis_prompt(cur, round_no, self.panel_runs(cur, round_no), feedback)
            elif member is not None:
                latest = self.panel_runs(cur, round_no).get(member['label'])
                if latest is not None and (not latest['handled'] or self.panel_success(latest)):
                    return None
                allowed, _ = heads.allowed_vendors(self.conn, self.cfg, cur['step'], limits, self.clock(), tag=cur['tag'])
                if lane_active >= self.admission_limit(slots, urgent) or vendor not in allowed:
                    return None
                claim['member'] = member['label']
                feedback = cur['feedback']
                if latest is not None:
                    feedback = (feedback + '\n' if feedback else '') + f"Your previous attempt failed: {latest['error'] or latest['outcome'] or latest['status']}"
                prompt = heads.worker_prompt(cur, round_no, feedback, [], report_file=report_file)
                prompt += f"\n\nPanel member {member['label']}. Work independently; do not read other member directories or reports."
            elif role == 'worker':
                steers = store.steers(self.conn, c['id'])
                handoff = self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='steer_handoff' AND json_extract(data, '$.round')=?",
                                            (c['id'], cur['round'])).fetchone()
                waited = self.conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='process_ended' AND json_extract(data, '$.round')=?",
                                           (c['id'], cur['round'])).fetchone()
                prompt = heads.worker_prompt(cur, round_no, cur['feedback'], steers, stopped=cur['round'] if handoff else None,
                                             waited=cur['round'] if waited else None)
                owed = [s['id'] for s in api.steer_deliveries(self.conn, c['id']) if s['delivery'] != 'acknowledged']
                if owed:
                    claim['steers'] = owed
            else:
                w = self.conn.execute("SELECT report FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC", (c['id'], c['round'])).fetchone()
                g = self.conn.execute('SELECT * FROM gates WHERE card_id=? AND round=?', (c['id'], c['round'])).fetchone()
                prompt = heads.reviewer_prompt(cur, round_no, w['report'] if w else '', g or {'exit_code': None, 'output': ''})
            bypass = self.pressure_bypass(urgent) or self.cap_bypass(urgent, lane_active, slots)
            try:
                rcur = self.conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,task_type,prompt,status,started_at,run_dir,member,report_file)
                                            VALUES(?,?,?,?,?,?,?,?,?,'starting',?,?,?,?)""",
                                         (c['id'], round_no, role, vendor, head['model'], head['effort'], c['step'],
                                          c['task_type'], prompt, now, str(run_dir), store.SYNTHESIS if synthesis else member['label'] if member else None, report_file))
                if self.launcher == 'systemd':  # name known before spawn: a crash mid-spawn stays detectable
                    self.conn.execute('UPDATE runs SET unit=? WHERE id=?', (f'hive-run-{rcur.lastrowid}', rcur.lastrowid))
            except Exception as e:
                store.event(self.conn, c['id'], 'claim_conflict', {'error': str(e)})
                return None
            rid = rcur.lastrowid
            if role == 'worker':
                if not store.cas_card(self.conn, cur, 'running', round=round_no, workdir=str(root), executor_wait=None,
                                      retry_at=None):
                    raise RuntimeError('CAS lost under write lock')
                if cur['assignee'] not in (None, vendor):
                    fallback = {
                        'run': rid, 'round': round_no, 'assigned': cur['assignee'], 'actual': vendor,
                        'reason': fallback_reason or why[cur['assignee']]}
                    if cur['model'] is not None:
                        fallback.update(pinned_model=cur['model'], actual_model=head['model'])
                    store.event(self.conn, c['id'], 'assignee_fallback', fallback)
                if cur['executor'] is not None:
                    store.event(self.conn, c['id'], 'executor_fallback', {
                        'run': rid, 'round': round_no, 'executor': cur['executor'], 'vendor': vendor,
                        'model': head['model'], 'reason': executor_reason})
                episode, _ = store.meta_get(self.conn, 'provider_outage_episode')
                if episode is not None:
                    self.queue_provider_episode(c['id'], episode, 'recovery', [vendor])
                    self.conn.execute("DELETE FROM meta WHERE key='provider_outage_episode'")
            store.event(self.conn, c['id'], 'claimed', {'run': rid, **claim, 'admission_class': api.admission_class(cur, self.cfg),
                                                        'bypass': bypass})
            if urgent:
                self.record_urgent(c['id'], rid, bypass)
            if member is not None:
                self.conn.execute('DELETE FROM meta WHERE key=?', (f"panel_wait:{c['id']}:{member['label']}",))
        job = {'argv': argv, 'cwd': str(workdir), 'env': heads.head_env(self.cfg, vendor, c['tag']), 'prompt': prompt,
               'timeout': self.cfg['timeouts'].get(c['step'], 3600), 'vendor': vendor, 'role': role,
               'report_file': report_file, 'live_input': live_input,
               'model': head['model'], 'effort': head['effort'], 'deny_tools': json.loads(c['deny_tools'])}
        (run_dir / 'job.json').write_text(json.dumps(job))
        os.chmod(run_dir / 'job.json', 0o600)
        try:
            self.spawn(rid, workdir)
        except Exception as e:
            with store.tx(self.conn):
                self.conn.execute("UPDATE runs SET status='failed_launch', ended_at=?, outcome='failed_launch', error=? WHERE id=? AND status='starting'",
                                  (self.clock(), f'{type(e).__name__}: {e}', rid))
            return rid
        return rid

    def head_vendors(self, c, role, member):
        """The vendors the pick of this head may choose once no budget, cooldown or limit stands in the way."""
        if member is not None:
            return [member['vendor']]
        if role == 'reviewer':
            worker = self.conn.execute("SELECT vendor FROM runs WHERE card_id=? AND round=? AND role='worker' ORDER BY id DESC",
                                       (c['id'], c['round'])).fetchone()
            if worker is not None and worker['vendor'] in heads.OTHER:
                same = [worker['vendor']] if self.cfg.get('review_same_vendor_fallback') else []
                return [heads.OTHER[worker['vendor']], *same]
        elif c['assignee'] is not None and c['on_unavailable'] == 'wait':
            return [c['assignee']]
        return list(heads.VENDORS)

    # ── external executors ─────────────────────────────────────────────────
    def external(self, name, *args):
        """Run one call of an external executor client; never a blocking wait for its answer."""
        ec = self.cfg['executors'][name]
        return subprocess.run(list(ec['command']) + list(args), capture_output=True, text=True,
                              timeout=ec['call_timeout_s'], stdin=subprocess.DEVNULL)

    def admit_external(self, name):
        """The only gate before an external `send`: None when the executor may take a card now, else the reason."""
        ec, now = self.cfg['executors'][name], self.clock()
        cooldown, _ = store.meta_get(self.conn, f'executor_cooldown:{name}')
        if cooldown and cooldown['until'] > now:
            return f"{name} cooldown until {cooldown['until']:.0f} ({cooldown['reason']})"
        active = self.conn.execute("SELECT count(*) FROM runs WHERE vendor=? AND status IN ('starting','running')",
                                   (name,)).fetchone()[0]
        if active >= ec['max_parallel']:
            return f"{name} max_parallel {ec['max_parallel']} reached"
        if heads.runs_today(self.conn, name, self.cfg['owner_tz'], now) >= ec['daily_sends']:
            return f"{name} daily_sends {ec['daily_sends']} reached"
        return None

    def head_round_reason(self, c):
        """Why a queued external-executor card runs its next worker on a head instead: the fallback round after an
        executor timeout, or a head round requeued by provider failover or outage. None sends to the executor."""
        if c['on_unavailable'] != 'fallback':
            return None
        prior = self.conn.execute("SELECT * FROM runs WHERE card_id=? AND role='worker' ORDER BY id DESC LIMIT 1",
                                  (c['id'],)).fetchone()
        if prior is None:
            return None
        if prior['vendor'] == c['executor']:
            return prior['error'] if prior['status'] == 'timeout' else None
        if self.conn.execute("""SELECT 1 FROM events WHERE card_id=? AND json_extract(data, '$.run')=?
                                AND kind IN ('provider_failover','provider_outage_wait')""",
                             (c['id'], prior['id'])).fetchone():
            return f"head round {prior['round']} continues after provider failure {prior['provider_failure']}"
        return None

    def start_cooldown(self, name, card_id, run_id):
        """Inside a tx. Open an auth cooldown of the executor unless one is active; journal each episode once."""
        key, now = f'executor_cooldown:{name}', self.clock()
        cooldown, _ = store.meta_get(self.conn, key)
        if cooldown and cooldown['until'] > now:
            return
        until = now + self.cfg['executors'][name]['cooldown_s']
        store.meta_set(self.conn, key, {'until': until, 'reason': 'auth'})
        store.event(self.conn, card_id, 'executor_cooldown', {'executor': name, 'run': run_id, 'reason': 'auth',
                                                              'until': until})

    def send_external(self, c):
        """Claim one round and send the card once. The run is committed before the send, so a run
        still 'starting' without a job id may have been sent and is never sent again. Exit 3 sent nothing:
        it opens a cooldown and returns the card to the queue of the same round without spending it."""
        name, round_no, now = c['executor'], c['round'] + 1, self.clock()
        root = heads.prepare_workspace(self.cfg, c)
        prompt = heads.card_text(c)
        if c['feedback']:
            prompt += '\n\n## Previous round failed. Fix this:\n' + c['feedback']
        prompt += '\n\n' + self.cfg['texts']['report_request']
        run_dir = root / '.hive' / f'r{round_no}-worker-{name}-{uuid.uuid4().hex}'
        run_dir.mkdir(parents=True)
        with store.tx(self.conn):
            cur = self.conn.execute('SELECT * FROM cards WHERE id=?', (c['id'],)).fetchone()
            if cur['version'] != c['version'] or cur['state'] != 'queued':
                return None
            rid = self.conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,task_type,prompt,status,
                                                       started_at,run_dir,report_file)
                                        VALUES(?,?,'worker',?,?,'external',?,?,?,'starting',?,?,'REPORT.md')""",
                                     (c['id'], round_no, name, name, c['step'], c['task_type'], prompt, now,
                                      str(run_dir))).lastrowid
            if not store.cas_card(self.conn, cur, 'running', round=round_no, workdir=str(root), executor_wait=None,
                                  retry_at=None):
                raise RuntimeError('CAS lost under write lock')
            store.event(self.conn, c['id'], 'claimed', {'run': rid, 'role': 'worker', 'round': round_no,
                                                        'executor': name,
                                                        'admission_class': api.admission_class(cur, self.cfg)})
        try:
            p = self.external(name, 'send', prompt)
        except subprocess.TimeoutExpired:
            self.end_external(rid, 'crashed', None, SEND_UNKNOWN, f'send outcome unknown: {name} send timed out')
            return rid
        except OSError as e:
            self.end_external(rid, 'failed_launch', None, 'failed_launch', f'{type(e).__name__}: {e}')
            return rid
        job = p.stdout.strip()
        if p.returncode == 3:
            self.unsent_external(rid, f'{name} send exit 3: {(p.stderr or p.stdout)[-500:]}')
        elif p.returncode != 0:
            self.end_external(rid, 'failed_launch', p.returncode, 'failed_launch',
                              f'{name} send exit {p.returncode}: {(p.stderr or p.stdout)[-500:]}')
        elif not job or len(job.split()) != 1:
            self.end_external(rid, 'crashed', p.returncode, SEND_UNKNOWN,
                              f'send outcome unknown: {name} send printed no job id: {p.stdout[-200:]!r}')
        else:
            with store.tx(self.conn):
                self.conn.execute("UPDATE runs SET job=?, status='running' WHERE id=? AND status='starting'", (job, rid))
                store.event(self.conn, c['id'], 'external_sent', {'run': rid, 'executor': name, 'job': job})
        return rid

    def unsent_external(self, rid, error):
        """End a run whose send was refused before anything was sent and requeue its card in the previous round."""
        now = self.clock()
        with store.tx(self.conn):
            r = self.conn.execute('SELECT * FROM runs WHERE id=?', (rid,)).fetchone()
            self.conn.execute("""UPDATE runs SET status='failed_launch', exit_code=3, ended_at=?,
                                 duration_s=round(?-started_at, 1), outcome='auth', error=?, handled=1
                                 WHERE id=? AND status='starting'""", (now, now, error, rid))
            c = self.conn.execute('SELECT * FROM cards WHERE id=?', (r['card_id'],)).fetchone()
            if not store.cas_card(self.conn, c, 'queued', round=r['round'] - 1):
                raise RuntimeError('CAS lost under write lock')
            store.event(self.conn, c['id'], 'external_unsent', {'run': rid, 'executor': r['vendor'], 'error': error})
            self.start_cooldown(r['vendor'], c['id'], rid)

    def end_external(self, rid, status, code, outcome, error, report=None, files=()):
        """Finish an external run once; a report is journalled as a fresh file report of this run."""
        now = self.clock()
        with store.tx(self.conn):
            cur = self.conn.execute("""UPDATE runs SET status=?, exit_code=?, ended_at=?, duration_s=round(?-started_at, 1),
                                       outcome=?, error=?, report=?, report_sha256=?, report_tail=? WHERE id=? AND status IN ('starting','running')""",
                                    (status, code, now, now, outcome, error, report,
                                     hashlib.sha256(report.encode('utf-8')).hexdigest() if report is not None else None,
                                     report[-EXCERPT:] if report is not None else None, rid))
            if cur.rowcount and report is not None:
                card_id = self.conn.execute('SELECT card_id FROM runs WHERE id=?', (rid,)).fetchone()['card_id']
                store.event(self.conn, card_id, 'run_report', {'run': rid, 'file': 'REPORT.md', 'source': 'file',
                                                               'files': list(files)})

    def poll_external(self, now):
        """One non-blocking `get` per external run whose poll interval elapsed; a ready answer becomes REPORT.md.
        A run left 'starting' by an earlier tick ends with an unknown send outcome; a run older than the executor's
        timeout_s ends as 'timeout'. A failed `get` keeps the run polling and holds its class and text in
        outcome/error until the next answered poll."""
        n = 0
        for r in self.conn.execute("""SELECT r.*, c.executor, c.workdir FROM runs r JOIN cards c ON c.id=r.card_id
                                      WHERE r.status IN ('starting','running') AND r.vendor = c.executor
                                      ORDER BY r.id""").fetchall():
            name = r['executor']
            ec = self.cfg['executors'][name]
            if r['status'] == 'starting':
                self.end_external(r['id'], 'crashed', None, SEND_UNKNOWN,
                                  f'send outcome unknown: run {r["id"]} has no {name} job id after a restart')
                n += 1
                continue
            if now - r['started_at'] > ec['timeout_s']:
                self.end_external(r['id'], 'timeout', None, 'timeout',
                                  f'{name} job {r["job"]} unanswered after {ec["timeout_s"]}s')
                n += 1
                continue
            if now - (r['heartbeat_at'] or r['started_at']) < ec['poll_s']:
                continue
            n += 1
            try:
                p = self.external(name, 'get', r['job'], '--json')
            except (OSError, subprocess.TimeoutExpired) as e:
                self.poll_failed(r, now, 'error', f'{name} get: {type(e).__name__}: {e}')
                continue
            if p.returncode == 2:
                with store.tx(self.conn):
                    self.conn.execute("UPDATE runs SET heartbeat_at=?, outcome=NULL, error=NULL WHERE id=? AND status='running'",
                                      (now, r['id']))
                continue
            if p.returncode != 0:
                self.poll_failed(r, now, 'auth' if p.returncode == 3 else 'error',
                                 f'{name} get exit {p.returncode}: {(p.stderr or p.stdout)[-500:]}')
                continue
            try:
                answer = json.loads(p.stdout)
                root = Path(r['workdir'])
                for f in answer['files']:
                    shutil.copy2(f, root / Path(f).name)
                (root / 'REPORT.md').write_text(answer['text'])
            except (ValueError, KeyError, TypeError, OSError) as e:
                self.poll_failed(r, now, 'error', f'{name} answer unusable: {type(e).__name__}: {e}')
                continue
            self.end_external(r['id'], 'exited', 0, 'ok', None, report=answer['text'],
                              files=[Path(f).name for f in answer['files']])
        return n

    def poll_failed(self, r, now, cls, error):
        """Keep the run polling after a failed `get`; journal the failure unless it repeats the previous one.
        An auth failure (exit 3) opens the executor cooldown."""
        with store.tx(self.conn):
            cur = self.conn.execute("UPDATE runs SET heartbeat_at=?, outcome=?, error=? WHERE id=? AND status='running'",
                                    (now, cls, error, r['id']))
            if cur.rowcount and (r['outcome'], r['error']) != (cls, error):
                store.event(self.conn, r['card_id'], 'external_poll_failed',
                            {'run': r['id'], 'executor': r['executor'], 'job': r['job'], 'class': cls, 'error': error})
            if cur.rowcount and cls == 'auth':
                self.start_cooldown(r['executor'], r['card_id'], r['id'])

    def wait_for_panel_member(self, c, member, reason):
        """Record each change of a panel member's admission reason without blocking its peers."""
        key = f"panel_wait:{c['id']}:{member['label']}"
        with store.tx(self.conn):
            prior, _ = store.meta_get(self.conn, key)
            if prior != reason:
                store.meta_set(self.conn, key, reason)
                store.event(self.conn, c['id'], 'panel_wait', {'member': member['label'], 'reason': reason})

    def wait_for_executor(self, c, executor, reason):
        """Record that the queued card waits for its unavailable assignee or external executor; one event per
        change of reason."""
        wait = {'executor': executor, 'reason': reason}
        if c['executor_wait'] is not None and json.loads(c['executor_wait']) == wait:
            return
        with store.tx(self.conn):
            if store.cas_card(self.conn, c, 'queued', executor_wait=json.dumps(wait, ensure_ascii=False)):
                store.event(self.conn, c['id'], 'executor_wait', wait)

    def spawn(self, run_id, workdir):
        runner = [sys.executable, '-m', 'hive.runner', '--db', self.cfg['db'], '--run-id', str(run_id),
                  '--config', self.cfg['_path']]
        if self.launcher == 'systemd':
            unit = f'hive-run-{run_id}'
            subprocess.run(['systemd-run', '--user', '--quiet', '--collect', f'--unit={unit}',
                            f'--working-directory={workdir}', f'--setenv=PYTHONPATH={REPO}',
                            '-p', 'KillMode=control-group', '-p', 'Nice=10',
                            '-p', 'IOSchedulingClass=idle', '-p', 'IOWeight=10',
                            '-p', 'MemoryHigh=768M', '-p', 'MemoryMax=1280M',
                            '--'] + runner, check=True, capture_output=True, timeout=30)
            return unit
        subprocess.Popen(resources.background(runner), cwd=str(workdir), env=dict(os.environ, PYTHONPATH=str(REPO)),
                         stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                         start_new_session=True)
        return None

    # ── events to the secretary ───────────────────────────────────────────────
    def adapter(self):
        if self._adapter is None:
            self._adapter = delivery.open_adapter(self.cfg)
        return self._adapter

    def publish(self, event_id=None):
        n = 0
        for o in self.conn.execute('SELECT * FROM outbox WHERE published_at IS NULL AND (? IS NULL OR event_id=?) ORDER BY created_at', (event_id, event_id)).fetchall():
            try:
                card = self.conn.execute('SELECT * FROM cards WHERE id=?', (o['card_id'],)).fetchone()
                destination = card['deliver'] if card is not None else 'owner-inbox'
                # Background results are secretary input.
                # Apply at publication as well as creation, including old outbox rows
                # and watch gates which still carry the legacy destination.
                if destination == 'telegram':
                    destination = 'owner-inbox'
                payload = json.loads(o['payload'])
                if o['kind'].startswith('provider_') or payload.get('loop_stopped') or payload.get('state') == 'missed':
                    destination = 'owner-inbox'  # global provider episodes keep their existing inbox contract
                # Pending pre-deploy rows obey the same explicit metadata contract.
                result = payload.get('result', {})
                if (destination == 'owner-inbox' and card is not None
                        and card['card_class'] == 'series_run' and not card['owner_visible']
                        and o['kind'] == 'task_result' and payload.get('state') == 'done'
                        and not payload.get('loop_stopped')
                        and result.get('news') is not True
                        and result.get('series_stopped') is not True
                        and not card['series_stopped']):
                    destination = 'local'
                if destination == 'owner-inbox':
                    self.deliver(o)
            except Exception as e:
                with store.tx(self.conn):
                    self.conn.execute('UPDATE outbox SET error=? WHERE event_id=?', (f'{type(e).__name__}: {e}', o['event_id']))
                continue
            with store.tx(self.conn):
                cur = self.conn.execute('UPDATE outbox SET published_at=?, error=NULL WHERE event_id=? AND published_at IS NULL',
                                        (self.clock(), o['event_id']))
                if not cur.rowcount:
                    continue  # the detached gate worker or a tick already published this row
                if destination == 'owner-inbox':
                    store.event(self.conn, o['card_id'], 'secretary_event', {'event_id': o['event_id'], 'kind': o['kind']})
                else:
                    store.event(self.conn, o['card_id'], 'result_delivered', {'event_id': o['event_id'], 'destination': destination})
            n += 1
        return n

    def deliver(self, o):
        """Hand outbox row `o` to the delivery adapter: its payload with the card's class and visibility, the
        summaries bounded and the last worker report as `text`."""
        payload = json.loads(o['payload'])
        card = self.conn.execute('SELECT card_class,owner_visible FROM cards WHERE id=?', (o['card_id'],)).fetchone()
        if card is not None:
            payload.update(card_class=card['card_class'], owner_visible=bool(card['owner_visible']))
        # Bound duplicated summary fields before adding worker text (64 KiB wire limit).
        for field, limit in (('result_line', 2000), ('report_excerpt', 2500)):
            if isinstance(payload.get(field), str):
                payload[field] = payload[field][:limit]
        if card is not None:
            run = self.conn.execute("SELECT report FROM runs WHERE card_id=? AND role='worker' ORDER BY id DESC LIMIT 1", (o['card_id'],)).fetchone()
            if run is not None and run['report']:
                payload['text'] = run['report'][:8000]
                payload['text_truncated'] = len(run['report']) > 8000
        self.adapter().deliver(delivery.Event(o['kind'], o['event_id'], json.dumps(payload, ensure_ascii=False),
                                              o['source_ts']))


def observe(conn, cfg, now=None):
    """Anomalies for the observer, including created holds beyond the configured limit."""
    now = now or time.time()
    out = []
    tick, at = store.meta_get(conn, 'dispatcher_tick')
    if at is None or now - at > 300:
        out.append({'kind': 'dispatcher_silent', 'last_tick_age_s': None if at is None else int(now - at)})
    for c in conn.execute("""SELECT c.*, (SELECT vendor FROM runs WHERE card_id=c.id AND role='worker' ORDER BY id DESC LIMIT 1)
                             AS worker_vendor FROM cards c WHERE state IN ('running','check','review')""").fetchall():
        external = c['executor'] is not None and c['worker_vendor'] == c['executor']
        limit = (c['script_timeout'] if c['script'] else cfg['executors'][c['executor']]['timeout_s'] if external else cfg['timeouts'].get(c['step'], 3600)) * 1.5
        # A judge retry waits for the gateway's time; the age starts when it falls due.
        since = max(c['updated_at'], c['retry_at']) if c['state'] == 'check' and c['retry_at'] is not None else c['updated_at']
        if now - since > limit:
            out.append({'kind': 'stuck', 'card': c['id'], 'state': c['state'], 'age_s': int(now - since)})
    for c in conn.execute("SELECT * FROM cards WHERE state='queued'").fetchall():
        # A retry backoff is a scheduled wait; the queue age starts when it falls due.
        since = max(c['updated_at'], c['retry_at']) if c['retry_at'] is not None else c['updated_at']
        if now - since > 3600:
            out.append({'kind': 'queued_long', 'card': c['id'], 'age_s': int(now - since)})
    hold_limit = cfg['held_long_s']
    for c in conn.execute("""SELECT id, title, acceptance, created_at FROM cards
                             WHERE state='created' AND not_before>? AND recheck_every IS NULL""", (now,)).fetchall():
        age = now - c['created_at']
        if age > hold_limit:
            out.append({'kind': 'held_long', 'card': c['id'], 'age_s': int(age), 'title': c['title'],
                        'acceptance_tail': c['acceptance'][-EXCERPT:]})
    for c in conn.execute("SELECT id, block_reason FROM cards WHERE state='blocked' AND finished_at>?", (now - 86400,)).fetchall():
        out.append({'kind': 'blocked_24h', 'card': c['id'], 'reason': c['block_reason']})
    for c in conn.execute("SELECT id, executor_wait FROM cards WHERE state='queued' AND executor_wait IS NOT NULL").fetchall():
        wait = json.loads(c['executor_wait'])
        out.append({'kind': 'executor_wait', 'card': c['id'], 'executor': wait['executor'], 'reason': wait['reason']})
    for r in conn.execute("""SELECT r.card_id, r.vendor, r.outcome, r.error FROM runs r JOIN cards c ON c.id=r.card_id
                             WHERE r.status='running' AND r.vendor=c.executor AND r.error IS NOT NULL""").fetchall():
        out.append({'kind': 'external_poll_failed', 'card': r['card_id'], 'executor': r['vendor'],
                    'class': r['outcome'], 'error': r['error']})
    for o in conn.execute('SELECT * FROM outbox WHERE published_at IS NULL').fetchall():
        out.append({'kind': 'event_unpublished', 'card': o['card_id'], 'error': o['error']})
    for e in conn.execute("""SELECT e.card_id, e.at, r.id AS run FROM events e JOIN runs r ON r.id=json_extract(e.data, '$.run')
                             WHERE e.kind='steer_stop' AND r.status IN ('starting','running')""").fetchall():
        if now - e['at'] > STOP_GRACE_S:
            out.append({'kind': 'steer_stop_stuck', 'card': e['card_id'], 'run': e['run'], 'age_s': int(now - e['at'])})
    return out
