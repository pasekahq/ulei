"""Read-only snapshots of the hive for the secretary: one card from the inside (`card`), the hive as a whole
(`health`), a filtered card search (`find`) and one file of a card's workspace (`read_file`).

Each is an instant picture taken on request: nothing is written, nothing is streamed, no head is touched. The
process facts (`pulse`) come from the unit cgroups of the user manager and /proc, the deliveries from the delivery
adapter's `status`, the pull request checks from the `gh` command when one is configured. Every text is cut to a
cap so one snapshot stays a few hundred KiB at most."""
import json
import os
import re
import subprocess
from pathlib import Path

from . import api, heads as heads_mod, liveness, store

VERSION = 1
REPORT_CHARS = 6000       # a run's report and a judge's reason in the snapshot; the whole file through read_file
OUTPUT_CHARS = 2000       # a gate's output
GOAL_CHARS = 4000
STEER_CHARS = 4000
CMDLINE_CHARS = 1500     # a command line as the snapshot shows it, after its preamble
SCAN_CHARS = 16384        # the part of a command line searched for units, pull requests and workflow runs
MAX_PROCS = 64            # processes listed per unit
MAX_LINKED_UNITS = 8      # other units a head's command lines name
MAX_PULLS = 3             # pull requests looked up per snapshot
GH_TIMEOUT_S = 20
FILE_CHARS = 256 * 1024   # the most bytes read_file returns per call
LIST_ENTRIES = 200        # entries listed per workspace directory
QUEUE_SHOWN = 30
DISPATCHER_STALE_S = 120  # a tick older than this: the dispatcher is not ticking
SECRET_NAME = re.compile(r'(^|[._-])(env|secrets?|token|credentials?|passwords?|key|pem|netrc)([._-]|$)|^id_[a-z0-9]+$',
                         re.IGNORECASE)
# The events whose `data` explains a round's outcome.
ROUND_EVENTS = ('gate', 'judge', 'review_requested', 'retry', 'blocked', 'done', 'cancelled', 'rework', 'process_wait',
                'run_reaped', 'launch_error', 'steer_restart')
PULL = [re.compile(r'\bgh\s+pr\s+(?:checks|view|merge|ready|diff)\s+(?:(?:-R|--repo)[= ]?\S+\s+)?#?(\d+)\b'),
        re.compile(r'github\.com/([\w.-]+/[\w.-]+)/pull/(\d+)')]
RUN_WATCH = re.compile(r'\bgh\s+run\s+(?:watch|view)\s+(?:(?:-R|--repo)[= ]?\S+\s+)?(\d+)\b')
# What a vendor CLI puts before the command a head asked for (sourcing its shell snapshot, unaliasing): the snapshot
# shows the command from its `eval '`.
PREAMBLE = re.compile(r"^/bin/(?:ba)?sh -c source \S+ .*?&& eval '", re.DOTALL)
REPO_FLAG = re.compile(r'(?:--repo[= ]|-R\s+)([\w.-]+/[\w.-]+)')
# A unit name as a word of its own: not part of a path (`/srv/name/x`) or of an `owner/name` repository.
UNIT_TOKEN = re.compile(r'(?<![\w@.:/-])[A-Za-z0-9@_.:-]+(?![\w@.:/-])')


def cut(text, n):
    if text is None:
        return None
    return text if len(text) <= n else text[:n] + f'\n… [{len(text) - n} more characters]'


def loads(raw):
    return json.loads(raw) if raw else None


# ── processes ─────────────────────────────────────────────────────────────────

class Probe:
    """The process facts of a snapshot: unit cgroups under `cgroup_root`, processes under `proc_root`, pull
    requests through `gh` (`gh` None: no lookup). Tests hand fakes of the same shape."""

    def __init__(self, cgroup_root=liveness.CGROUP_ROOT, proc_root=liveness.PROC_ROOT, gh=('gh',), now=None):
        self.cgroup_root, self.proc_root, self.gh = Path(cgroup_root), Path(proc_root), gh
        self._units = None
        try:
            self.uptime = float((self.proc_root / 'uptime').read_text().split()[0])
        except (OSError, ValueError, IndexError):
            self.uptime = None
        self.hz = os.sysconf('SC_CLK_TCK') if hasattr(os, 'sysconf') else 100

    def units(self):
        """Names (without `.service`) of the services in every slice of the user manager, read once."""
        if self._units is None:
            found, pending = set(), [liveness.manager_cgroup(self.cgroup_root)]
            while pending:
                slice_ = pending.pop()
                try:
                    children = list(slice_.iterdir())
                except OSError:
                    continue
                for child in children:
                    if child.name.endswith('.service') and child.is_dir():
                        found.add(child.name[:-len('.service')])
                    elif child.name.endswith('.slice') and child.is_dir():
                        pending.append(child)
            self._units = found
        return self._units

    def unit_pids(self, unit):
        """(populated, pids) of the unit's cgroup; (None, []) when it has none."""
        cgroup = liveness.unit_cgroup(self.cgroup_root, unit)
        if cgroup is None:
            return None, []
        try:
            events = (cgroup / 'cgroup.events').read_text()
            pids = sorted({int(p) for p in (cgroup / 'cgroup.procs').read_text().split()})
        except (OSError, ValueError):
            return None, []
        return 'populated 1' in events.splitlines(), pids

    def process(self, pid):
        """{pid, ppid, cmdline, age_s, cwd} of a live process, None when it is gone or a zombie."""
        base = self.proc_root / str(pid)
        try:
            raw = (base / 'cmdline').read_bytes()
            stat = (base / 'stat').read_text()
        except OSError:
            return None
        if not raw:
            return None
        rest = stat.rsplit(')', 1)[1].split()
        if rest[0] == 'Z':
            return None
        age = None
        if self.uptime is not None:
            try:
                age = round(self.uptime - int(rest[19]) / self.hz, 1)
            except (ValueError, IndexError):
                age = None
        try:
            cwd = os.readlink(base / 'cwd')
        except OSError:
            cwd = None
        full = raw.rstrip(b'\0').replace(b'\0', b' ').decode(errors='replace')[:SCAN_CHARS]
        shown = PREAMBLE.sub('', full, count=1)
        return {'pid': pid, 'ppid': int(rest[1]), 'cmdline': shown[:CMDLINE_CHARS], 'scan': full, 'age_s': age,
                'cwd': cwd}

    def pull(self, number, repo, cwd):
        """The pull request's state and checks through `gh pr view`; {'error': ...} when it cannot be read."""
        argv = ['pr', 'view', str(number), '--json', 'number,url,state,mergeStateStatus,statusCheckRollup']
        return gh_json(self, argv + (['--repo', repo] if repo else []), cwd)

    def workflow_run(self, run_id, repo, cwd):
        """The workflow run's status and conclusion through `gh run view`; {'error': ...} when it cannot be read."""
        argv = ['run', 'view', str(run_id), '--json', 'name,status,conclusion,url,headBranch,createdAt,updatedAt']
        return gh_json(self, argv + (['--repo', repo] if repo else []), cwd)


def gh_json(probe, argv, cwd):
    if not probe.gh:
        return {'error': 'no gh command configured'}
    try:
        out = subprocess.run([*probe.gh, *argv], cwd=cwd if cwd and os.path.isdir(cwd) else None, capture_output=True,
                             text=True, timeout=GH_TIMEOUT_S)
    except (OSError, subprocess.TimeoutExpired) as e:
        return {'error': f'gh: {e.__class__.__name__}: {e}'[:300]}
    if out.returncode != 0:
        return {'error': f'gh exit {out.returncode}: {out.stderr.strip()[:300]}'}
    try:
        return json.loads(out.stdout)
    except ValueError:
        return {'error': 'gh answered no JSON'}


def tree(probe, pids):
    """The live processes of `pids` with `depth` under the oldest ones of the set (0: no parent in the set)."""
    procs = [p for p in (probe.process(pid) for pid in pids[:MAX_PROCS]) if p is not None]
    by_pid = {p['pid']: p for p in procs}
    for p in procs:
        depth, parent = 0, p['ppid']
        while parent in by_pid and depth < 32:
            depth, parent = depth + 1, by_pid[parent]['ppid']
        p['depth'] = depth
        p['children'] = sum(1 for q in procs if q['ppid'] == p['pid'])
    return procs


def commands(procs, runner_pid):
    """The commands a head runs now: the processes two levels under its runner (runner → head → command); in a unit
    without the runner (one a head started), the unit's own root processes. Oldest first, each with its deepest descendant (`now`)."""
    by_pid = {p['pid']: p for p in procs}
    heads = {p['pid'] for p in procs if p['ppid'] == runner_pid}
    top = [p for p in procs if p['ppid'] in heads] if heads else [p for p in procs if p['depth'] == 0]
    out = []
    for p in sorted(top, key=lambda p: -(p['age_s'] or 0)):
        leaf = p
        for q in procs:
            chain, cur = [], q
            while cur is not None and cur['pid'] != p['pid']:
                chain.append(cur)
                cur = by_pid.get(cur['ppid'])
            if cur is not None and len(chain) > 0 and q['children'] == 0 and q['depth'] > leaf['depth']:
                leaf = q
        out.append({'pid': p['pid'], 'cmdline': p['cmdline'], 'age_s': p['age_s'],
                    'now': None if leaf is p else {'pid': leaf['pid'], 'cmdline': leaf['cmdline'], 'age_s': leaf['age_s']}})
    return out


def named_units(probe, procs, own):
    """Other services of the user manager the processes' command lines name (`systemctl --user is-active X`,
    `journalctl -u X`, a wait loop on X): the units a head started and waits on."""
    units = probe.units()
    found = []
    for p in procs:
        for token in UNIT_TOKEN.findall(p['scan']):
            name = token[:-len('.service')] if token.endswith('.service') else token
            if name != own and name in units and name not in found:
                found.append(name)
    return found[:MAX_LINKED_UNITS]


def pulls(procs):
    """[(number, repo or None, cwd, cmdline, age_s)] of the pull requests the command lines wait on or name."""
    seen, out = set(), []
    for p in procs:
        line = p['scan']
        repo = REPO_FLAG.search(line)
        repo = repo.group(1) if repo else None
        for pattern in PULL:
            for m in pattern.finditer(line):
                number, r = (int(m.group(1)), repo) if m.lastindex == 1 else (int(m.group(2)), m.group(1))
                if (number, r) not in seen:
                    seen.add((number, r))
                    out.append((number, r, p['cwd'], p['cmdline'], p['age_s']))
    return out


def checks_summary(view):
    """Counts of the pull request's checks by outcome, and the names of the failed and pending ones."""
    if 'error' in view:
        return view
    counts, failed, pending = {}, [], []
    for c in view.get('statusCheckRollup') or []:
        status = (c.get('status') or c.get('state') or '').upper()
        conclusion = (c.get('conclusion') or '').upper()
        name = c.get('name') or c.get('context') or '?'
        if status in ('COMPLETED', 'SUCCESS', 'FAILURE', 'ERROR') or conclusion:
            outcome = conclusion or status
        else:
            outcome = 'PENDING'
        counts[outcome] = counts.get(outcome, 0) + 1
        if outcome in ('FAILURE', 'ERROR', 'TIMED_OUT', 'CANCELLED', 'ACTION_REQUIRED', 'STARTUP_FAILURE'):
            failed.append(name)
        elif outcome == 'PENDING':
            pending.append(name)
    return {'number': view.get('number'), 'url': view.get('url'), 'state': view.get('state'),
            'merge_state': view.get('mergeStateStatus'), 'checks': counts, 'failed': failed[:20],
            'pending': pending[:20]}


def unit_view(probe, unit, runner_pid=None):
    populated, pids = probe.unit_pids(unit)
    procs = tree(probe, pids)
    return {'unit': unit, 'cgroup': populated is not None, 'populated': populated, 'processes': len(procs),
            'commands': commands(procs, runner_pid)}, procs


def pulse(conn, card, probe, now):
    """What the card's active heads do now: per run its unit, its processes and the commands it waits on, the other
    units those name, and the pull requests and checks they wait on."""
    runs = conn.execute("SELECT * FROM runs WHERE card_id=? AND status IN ('starting','running') ORDER BY id",
                        (card['id'],)).fetchall()
    heads, looked = [], 0
    for r in runs:
        entry = {'run_id': r['id'], 'role': r['role'], 'round': r['round'], 'vendor': r['vendor'], 'model': r['model'],
                 'effort': r['effort'], 'status': r['status'], 'unit': r['unit'], 'pid': r['pid'],
                 'started_at': r['started_at'], 'running_s': round(now - r['started_at'], 1),
                 'heartbeat_age_s': round(now - r['heartbeat_at'], 1) if r['heartbeat_at'] else None}
        procs = []
        if r['vendor'] == card['executor']:
            entry['external'] = card['executor']
        elif r['unit']:
            view, procs = unit_view(probe, r['unit'], r['pid'])
            entry.update(view)
        linked = []
        for name in named_units(probe, procs, r['unit']):
            view, sub = unit_view(probe, name)
            linked.append(view)
            procs = procs + sub
        entry['linked_units'] = linked
        waits = []
        for number, repo, cwd, line, age in pulls(procs):
            if looked >= MAX_PULLS:
                break
            looked += 1
            waits.append(dict(checks_summary(probe.pull(number, repo, cwd)), pr=number, repo=repo,
                              waiting_s=age, command=line))
        entry['pull_requests'] = waits
        runs_seen, wf = set(), []
        for p in procs:
            repo = REPO_FLAG.search(p['scan'])
            for m in RUN_WATCH.finditer(p['scan']):
                key = (int(m.group(1)), repo.group(1) if repo else None)
                if key in runs_seen or looked >= MAX_PULLS:
                    continue
                runs_seen.add(key)
                looked += 1
                wf.append(dict(probe.workflow_run(key[0], key[1], p['cwd']), run=key[0], repo=key[1],
                               waiting_s=p['age_s'], command=p['cmdline']))
        entry['workflow_runs'] = wf
        entry['waiting_on'] = waiting_on(entry)
        heads.append(entry)
    jobs = [dict(j) for j in conn.execute('SELECT version, mode, started_at FROM gate_jobs WHERE card_id=?',
                                          (card['id'],))]
    for j in jobs:
        j['running_s'] = round(now - j['started_at'], 1)
    return {'heads': heads, 'gate_jobs': jobs}


def waiting_on(head):
    """One line: the longest command the head itself runs and how long it has run, the units it names and the pull
    requests and workflow runs found in it or in those units. A long-running service the command names is listed
    under `linked_units`, not taken for what the head waits on."""
    cmds = head.get('commands') or []
    if not cmds:
        return None
    c = max(cmds, key=lambda c: c['age_s'] or 0)
    now = c['now']['cmdline'] if c.get('now') else c['cmdline']
    return {'unit': head.get('unit'), 'command': c['cmdline'], 'now': now, 'for_s': c['age_s'],
            'linked_units': [u['unit'] for u in head.get('linked_units') or []],
            'pull_requests': [p['pr'] for p in head.get('pull_requests') or []],
            'workflow_runs': [w['run'] for w in head.get('workflow_runs') or []]}


# ── one card ─────────────────────────────────────────────────────────────────

def links(conn, card_id):
    """The card's chain: parent and children, the card it follows up and its followups, blockers and the cards it
    blocks, its series."""
    out = {'parent': store.parent(conn, card_id), 'children': store.children(conn, card_id)}
    follow = conn.execute("""SELECT c.id, c.state, c.title FROM links l JOIN cards c ON c.id=l.target_id
                             WHERE l.card_id=? AND l.kind='followup'""", (card_id,)).fetchone()
    out['followup_of'] = dict(follow) if follow else None
    out['followups'] = [dict(r) for r in conn.execute(
        """SELECT c.id, c.state, c.title, c.created_at FROM links l JOIN cards c ON c.id=l.card_id
           WHERE l.target_id=? AND l.kind='followup' ORDER BY l.created_at""", (card_id,))]
    out['blocked_by'] = store.blockers(conn, card_id)
    out['blocks'] = [dict(r) for r in conn.execute(
        """SELECT c.id, c.state FROM links l JOIN cards c ON c.id=l.card_id
           WHERE l.target_id=? AND l.kind='blocked_by' ORDER BY l.rowid""", (card_id,))]
    series = conn.execute("""SELECT c.id, c.title FROM links l JOIN cards c ON c.id=l.target_id
                             WHERE l.card_id=? AND l.kind='series'""", (card_id,)).fetchone()
    out['series'] = dict(series) if series else None
    return out


def rounds(conn, card_id):
    """Per round: every run (who, model, effort, when, how it ended, its report), the gate's result and output, the
    judge's or reviewer's verdict and reason, and the round's retry, block or finish."""
    by_round = {}

    def slot(n):
        return by_round.setdefault(n, {'round': n, 'runs': [], 'gate': None, 'judge': None, 'events': []})
    for r in conn.execute('SELECT * FROM runs WHERE card_id=? ORDER BY id', (card_id,)):
        slot(r['round'])['runs'].append({
            'run_id': r['id'], 'role': r['role'], 'member': r['member'], 'vendor': r['vendor'], 'model': r['model'],
            'effort': r['effort'], 'step': r['step'], 'status': r['status'], 'unit': r['unit'], 'run_dir': r['run_dir'],
            'started_at': r['started_at'], 'ended_at': r['ended_at'], 'duration_s': r['duration_s'],
            'exit_code': r['exit_code'], 'outcome': r['outcome'], 'verdict': r['verdict'], 'error': r['error'],
            'provider_failure': r['provider_failure'], 'tokens_in': r['tokens_in'], 'tokens_out': r['tokens_out'],
            'report_file': r['report_file'], 'report': cut(r['report'], REPORT_CHARS)})
    for g in conn.execute('SELECT * FROM gates WHERE card_id=? ORDER BY id', (card_id,)):
        slot(g['round'])['gate'] = {'command': g['command'], 'passed': bool(g['passed']), 'exit_code': g['exit_code'],
                                    'output': cut(g['output'], OUTPUT_CHARS), 'at': g['at'], 'duration_s': g['duration_s']}
    current = 0
    marks = ','.join('?' * len(ROUND_EVENTS))
    for e in conn.execute(f"SELECT id, at, kind, actor, data FROM events WHERE card_id=? AND kind IN ('claimed',{marks}) "
                          'ORDER BY id', (card_id, *ROUND_EVENTS)):
        data = loads(e['data']) or {}
        n = data.get('round') if isinstance(data.get('round'), int) else current
        current = n
        if e['kind'] == 'claimed':
            continue
        if e['kind'] == 'judge':
            slot(n)['judge'] = {'verdict': data.get('verdict'), 'route': data.get('route'),
                                'reason': cut(data.get('reason'), REPORT_CHARS), 'at': e['at']}
            continue
        if e['kind'] == 'gate':
            continue
        slot(n)['events'].append({'kind': e['kind'], 'at': e['at'], 'actor': e['actor'],
                                  'data': {k: cut(v, OUTPUT_CHARS) if isinstance(v, str) else v
                                           for k, v in data.items()}})
    for entry in by_round.values():
        entry['verdict'] = verdict(entry)
    return [by_round[n] for n in sorted(by_round)]


def verdict(entry):
    """PASS or FAIL of a round and by whom, None while it has neither a gate, a judge nor a reviewer verdict."""
    judge, gate = entry['judge'], entry['gate']
    reviews = [r for r in entry['runs'] if r['role'] == 'reviewer' and r['verdict']]
    if judge and judge['verdict']:
        return {'result': 'PASS' if judge['verdict'] in ('done', 'pass', 'PASS') else 'FAIL', 'by': 'judge',
                'detail': judge['verdict']}
    if reviews:
        last = reviews[-1]['verdict']
        return {'result': 'PASS' if last.upper() == 'PASS' else 'FAIL', 'by': 'reviewer', 'detail': last}
    if gate:
        return {'result': 'PASS' if gate['passed'] else 'FAIL', 'by': 'gate', 'detail': f"exit {gate['exit_code']}"}
    return None


def deliveries(conn, card_id, adapter):
    """The card's events for the secretary: the hive's outbox row (published or not, its error) and, when the
    delivery adapter can read it, the owner inbox's row (status, attempts, outcome: the secretary's ACK)."""
    out = []
    for r in conn.execute('SELECT event_id, kind, created_at, published_at, error FROM outbox WHERE card_id=? '
                          'ORDER BY created_at', (card_id,)):
        out.append(dict(r, inbox=inbox_status(adapter, r['event_id']) if r['published_at'] else None))
    return out


def inbox_status(adapter, event_id):
    if adapter is None:
        return None
    try:
        return adapter.status(event_id)
    except Exception as e:  # noqa: BLE001 - a snapshot reports the failure instead of failing
        return {'error': f'{e.__class__.__name__}: {e}'[:300]}


def queue_order(conn, cfg, now):
    """The cards waiting for a head slot in the dispatcher's launch order: admission class, reviewers first, then
    priority and age; a card in backoff (`retry_at` ahead) is left out and listed by its card's `queue`."""
    rows = conn.execute("""SELECT * FROM cards c WHERE script IS NULL AND (
                             (state='review' AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.card_id=c.id AND r.round=c.round
                                                             AND r.role='reviewer'))
                             OR state='queued') AND (retry_at IS NULL OR retry_at<=?)""", (now,)).fetchall()
    classes = {name: i for i, name in enumerate(api.ADMISSION_CLASSES)}

    def key(c):
        return (classes[api.admission_class(c, cfg)], c['state'] != 'review', -c['priority'], c['created_at'])
    return sorted(rows, key=key)


def slot_holders(conn):
    """The runs holding a head slot now (any card, board or not: series runs, reviewers, synthesis) and the scripts."""
    rows = conn.execute("""SELECT r.id AS run_id, r.card_id, r.role, r.vendor, r.model, r.effort, r.unit, r.started_at,
                                  c.title, c.card_class, c.kind, c.executor, c.script
                           FROM runs r JOIN cards c ON c.id=r.card_id
                           WHERE r.status IN ('starting','running') ORDER BY r.started_at""").fetchall()
    heads = [dict(r) for r in rows if r['script'] is None and r['vendor'] != r['executor']]
    scripts = [dict(r) for r in rows if r['script'] is not None]
    external = [dict(r) for r in rows if r['script'] is None and r['vendor'] == r['executor']]
    for group in (heads, scripts, external):
        for r in group:
            r.pop('script', None)
    return heads, scripts, external


def queue(conn, cfg, card, now, cgroup_root=liveness.CGROUP_ROOT):
    """Why the card does not run now: its place on the board, its position in the launch queue, the holders of the
    slots, and its other waits (hold, backoff, blockers, a vendor's budget wait)."""
    off_board = card['kind'] == 'recurring' or (card['card_class'] == 'series_run' and not conn.execute(
        "SELECT 1 FROM links WHERE card_id=? AND kind='followup'", (card['id'],)).fetchone())
    if card['kind'] == 'recurring':
        column, wait = None, None  # the rule of a schedule or series: never on the board, never launched
    else:
        column, wait = api.placement(conn, card, now, lambda run: liveness.alive(run, cgroup_root))
    if off_board:
        shown = 'off_board'
    elif column is not None:
        shown = column
    else:
        shown = 'archive' if card['state'] in ('done', 'cancelled') else None
    out = {'column': shown,
           'wait': wait, 'admission_class': api.admission_class(card, cfg),
           'not_before': card['not_before'], 'retry_at': card['retry_at'], 'retry_streak': card['retry_streak'],
           'executor_wait': loads(card['executor_wait'])}
    if wait is not None:
        out['waiting_since'] = api.waiting_since(conn, card, now)
    if card['state'] in ('queued', 'review', 'created'):
        order = [c['id'] for c in queue_order(conn, cfg, now)]
        out['position'] = order.index(card['id']) + 1 if card['id'] in order else None
        out['queue_length'] = len(order)
        out['ahead'] = order[:out['position'] - 1][:QUEUE_SHOWN] if out['position'] else []
        heads, _, _ = slot_holders(conn)
        out['slots'] = slots(cfg, card, heads)
    # The dispatcher's last word on why no vendor took the card; kept after the card moved on, so only while it waits.
    budget, at = store.meta_get(conn, f"budget_wait:{card['id']}")
    waits = card['state'] in ('queued', 'review', 'created') or column == 'waiting'
    # A note older than the config file came from a dispatcher that ran the old limits: `stale`.
    mtime = config_mtime(cfg)
    out['budget_wait'] = ({'vendors': budget, 'at': at, 'age_s': round(now - at, 1),
                           'stale': bool(mtime and at < mtime)}
                          if budget and waits and not store.is_final(card) else None)
    return out


def slots(cfg, card, heads):
    urgent = api.admission_class(card, cfg) == 'urgent'
    reserve = cfg['admission']['urgent_reserve']
    ceiling = int(cfg.get('concurrency', 2))
    limit = ceiling + reserve if urgent else ceiling - reserve
    return {'concurrency': ceiling, 'urgent_reserve': reserve, 'limit_for_this_card': limit, 'used': len(heads),
            'free': max(0, limit - len(heads)),
            'holders': [{k: h[k] for k in ('run_id', 'card_id', 'role', 'card_class', 'started_at', 'title')}
                        for h in heads]}


def card(conn, cfg, card_id, now, probe=None, adapter=None):
    """The whole card from the inside at `now`; None for an unknown card. `probe` (Probe) reads the processes,
    `adapter` (the delivery adapter) the owner inbox."""
    c = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    if c is None:
        return None
    probe = probe or Probe()
    out = {'version': VERSION, 'now': now, 'card_id': c['id']}
    out['card'] = {k: c[k] for k in ('title', 'kind', 'card_class', 'owner_visible', 'state', 'wait_reason', 'round',
                                     'step', 'review', 'priority', 'tag', 'source', 'created_by', 'created_at',
                                     'updated_at', 'finished_at', 'acceptance', 'acceptance_kind', 'judge_criteria',
                                     'block_reason', 'result', 'assignee', 'executor', 'model', 'effort', 'workdir')}
    out['card']['goal'] = cut(c['goal'], GOAL_CHARS)
    out['card']['feedback'] = cut(c['feedback'], REPORT_CHARS)
    out['links'] = links(conn, card_id)
    out['rounds'] = rounds(conn, card_id)
    out['heads'] = pulse(conn, c, probe, now)
    out['queue'] = queue(conn, cfg, c, now, probe.cgroup_root)
    out['steers'] = [{'id': s['id'], 'by': s.get('by'), 'text': cut(s.get('text'), STEER_CHARS),
                      'after_round': s['after_round'], 'delivery': s['delivery'], 'method': s['method'],
                      'reason': s['reason'], 'run': s['run']} for s in api.steer_deliveries(conn, card_id)]
    out['deliveries'] = deliveries(conn, card_id, adapter)
    if c['kind'] == 'recurring':
        out['series'] = series_entry(conn, c, now)
    out['files'] = workspace(cfg, c)
    return out


# ── the workspace ─────────────────────────────────────────────────────────────

def results_names(cfg, c):
    """Names of files in the results directory the card's own goal, body or acceptance names."""
    root = cfg.get('results_dir')
    if not root:
        return []
    root = str(root).rstrip('/')
    text = '\n'.join(c[k] or '' for k in ('goal', 'body', 'acceptance'))
    names = []
    for m in re.finditer(re.escape(root) + r'/([\w.@+-][\w.@+/-]*)', text):
        name = m.group(1).rstrip('.')
        if name not in names:
            names.append(name)
    for m in re.finditer(r'(?<![\w/])(?:hive/)?out/([\w.@+-][\w.@+/-]*)', text):
        name = m.group(1).rstrip('.')
        if name not in names:
            names.append(name)
    return names[:LIST_ENTRIES]


def entry(path, rel):
    try:
        st = path.lstat()
    except OSError:
        return None
    kind = 'dir' if path.is_dir() and not path.is_symlink() else 'link' if path.is_symlink() else 'file'
    return {'path': rel, 'kind': kind, 'size': st.st_size if kind == 'file' else None, 'mtime': st.st_mtime,
            'readable': kind != 'link' and not SECRET_NAME.search(path.name)}


def workspace(cfg, c):
    """The card's workspace: its top level, `ev/` and `out/` one level down, its run directories' names, and the
    results files its goal names; every path is one `read_file` takes."""
    root = Path(c['workdir']) if c['workdir'] else None
    out = {'workdir': str(root) if root else None, 'exists': bool(root and root.is_dir()), 'entries': [],
           'results': []}
    if out['exists']:
        listed = []
        for sub in ('', 'ev', 'out', '.hive'):
            d = root / sub if sub else root
            if not d.is_dir() or d.is_symlink():
                continue
            try:
                names = sorted(os.listdir(d))[:LIST_ENTRIES]
            except OSError:
                continue
            for name in names:
                e = entry(d / name, f'{sub}/{name}' if sub else name)
                if e is not None and (sub or name not in ('ev', 'out', '.hive') or e['kind'] != 'dir'):
                    listed.append(e)
        out['entries'] = listed
    results = cfg.get('results_dir')
    for name in results_names(cfg, c):
        e = entry(Path(results) / name, f'@out/{name}')
        if e is not None:
            out['results'].append(e)
    return out


class FileError(Exception):
    """A path read_file refuses: outside the workspace, a symlink, a secret's name, or not a file."""


def resolve(cfg, c, rel):
    """The file `rel` names: a path inside the card's workspace, or `@out/<name>` for a results file the card's goal
    names. No `..`, no symlink anywhere on the way, no name that looks like a secret."""
    if not rel or '\0' in rel:
        raise FileError('empty path')
    if rel.startswith('@out/'):
        name = rel[len('@out/'):]
        if name not in results_names(cfg, c):
            raise FileError(f'{rel}: the card does not name this results file')
        base, parts = Path(str(cfg['results_dir'])), Path(name).parts
    else:
        if not c['workdir']:
            raise FileError('the card has no workspace')
        base, parts = Path(c['workdir']), Path(rel).parts
    if not parts or Path(rel).is_absolute() or any(p in ('..', '.', '') for p in parts):
        raise FileError(f'{rel}: only a relative path inside the workspace')
    path = base
    for p in parts:
        path = path / p
        if path.is_symlink():
            raise FileError(f'{rel}: a symlink is not followed')
        if SECRET_NAME.search(p):
            raise FileError(f'{rel}: the name looks like a secret')
    return path


def read_file(cfg, c, rel, offset=0, limit=FILE_CHARS):
    """{path, size, offset, text, truncated} of a file, or {path, entries} of a directory, of the card's workspace."""
    path = resolve(cfg, c, rel)
    if path.is_dir():
        names = sorted(os.listdir(path))[:LIST_ENTRIES]
        return {'path': rel, 'entries': [e for e in (entry(path / n, f'{rel.rstrip("/")}/{n}') for n in names) if e]}
    if not path.is_file():
        raise FileError(f'{rel}: no such file')
    limit = max(1, min(int(limit), FILE_CHARS))
    offset = max(0, int(offset))
    with path.open('rb') as f:
        size = os.fstat(f.fileno()).st_size
        f.seek(offset)
        raw = f.read(limit)
    return {'path': rel, 'size': size, 'offset': offset, 'text': raw.decode(errors='replace'),
            'truncated': offset + len(raw) < size}


# ── the hive as a whole ───────────────────────────────────────────────────────

def series_entry(conn, c, now):
    """A schedule or series: its rule, next start, run count and its last runs, and whether one holds a slot now."""
    full = api.get_schedule(conn, c)
    last = conn.execute("""SELECT c.id, c.state, c.series_period AS period, c.finished_at, c.result FROM links l
                           JOIN cards c ON c.id=l.card_id WHERE l.target_id=? AND l.kind='series'
                           ORDER BY c.series_period DESC LIMIT 3""", (c['id'],)).fetchall()
    active = conn.execute("""SELECT r.id FROM links l JOIN runs r ON r.card_id=l.card_id
                             WHERE l.target_id=? AND l.kind='series' AND r.status IN ('starting','running')""",
                          (c['id'],)).fetchall()
    return {'id': c['id'], 'title': c['title'], 'card_class': c['card_class'], 'schedule': full,
            'last_runs': [dict(r, result=cut(r['result'], 300)) for r in last],
            'holds_slot': [r['id'] for r in active]}


def series(conn, now, include_stopped=False):
    rows = conn.execute("SELECT * FROM cards WHERE state='blocked' AND kind='recurring' ORDER BY created_at").fetchall()
    out = [series_entry(conn, c, now) for c in rows if include_stopped or c['series_stopped'] is None]
    return out


CONFIG_KEYS = ('concurrency', 'script_concurrency', 'max_rounds', 'heartbeat_stale_s', 'spawn_grace_s', 'gate_timeout_s',
               'launcher', 'delivery', 'owner_tz', 'review_same_vendor_fallback')


def config_view(cfg):
    """The limits of the config, without paths, environments, commands or texts."""
    out = {k: cfg[k] for k in CONFIG_KEYS if k in cfg}
    out['admission'] = {k: v for k, v in (cfg.get('admission') or {}).items() if not isinstance(v, (dict,))}
    out['ladder'] = {str(step): {vendor: {k: v for k, v in pick.items() if k in ('model', 'effort')}
                                 for vendor, pick in vendors.items() if isinstance(pick, dict)}
                     for step, vendors in (cfg.get('ladder') or {}).items()}
    out['vendors'] = {name: {k: v[k] for k in ('disabled', 'weight', 'live_input', 'daily_runs', 'tag_launcher') if k in v}
                      for name, v in (cfg.get('vendors') or {}).items()}
    out['executors'] = {name: {k: v[k] for k in ('max_parallel', 'daily_sends', 'timeout_s') if k in v}
                        for name, v in (cfg.get('executors') or {}).items()}
    out['timeouts'] = {str(k): v for k, v in (cfg.get('timeouts') or {}).items()}
    out['budget'] = {k: v for k, v in (cfg.get('budget') or {}).items() if isinstance(v, (int, float, bool))}
    res = cfg.get('resources') or {}
    out['resources'] = {k: v for k, v in res.items() if isinstance(v, (int, float, bool))}
    out['judge'] = {'routes': {str(k): v for k, v in ((cfg.get('judge') or {}).get('routes') or {}).items()}}
    return out


def config_mtime(cfg):
    """When the config file last changed: a dispatcher started before it still runs the old values (budget_wait
    notes then name limits the file no longer has)."""
    try:
        return os.stat(cfg['_path']).st_mtime
    except (KeyError, OSError, TypeError):
        return None


def health(conn, cfg, now, adapter=None):
    """The hive at `now`: config limits, the dispatcher's last tick, slot holders, queue, series, the outbox."""
    tick, at = store.meta_get(conn, 'dispatcher_tick')
    gate, gate_at = store.meta_get(conn, 'resource_gate')
    limits, limits_at = store.meta_get(conn, 'limits')
    counts = {r['state']: r['n'] for r in conn.execute('SELECT state, count(*) AS n FROM cards GROUP BY state')}
    heads, scripts, external = slot_holders(conn)
    order = queue_order(conn, cfg, now)
    cooldowns = {r['key'].split(':', 1)[1]: loads(r['value']) for r in conn.execute(
        "SELECT key, value FROM meta WHERE key >= 'provider_cooldown:' AND key < 'provider_cooldown;'")}
    unpublished = conn.execute('SELECT count(*) FROM outbox WHERE published_at IS NULL').fetchone()[0]
    recent = [dict(r) for r in conn.execute('SELECT event_id, card_id, kind, created_at, published_at, error FROM outbox '
                                            'ORDER BY rowid DESC LIMIT 20')]
    for r in recent:
        r['inbox'] = inbox_status(adapter, r['event_id']) if r['published_at'] else None
    gate_jobs = [dict(r) for r in conn.execute('SELECT card_id, version, mode, started_at FROM gate_jobs')]
    return {
        'version': VERSION, 'now': now, 'config': config_view(cfg), 'config_mtime': config_mtime(cfg),
        'dispatcher': {'last_tick_at': at, 'tick_age_s': round(now - at, 1) if at else None,
                       'ticking': bool(at and now - at < DISPATCHER_STALE_S), 'last_tick': tick},
        'resource_gate': {'reason': (gate or {}).get('reason'), 'at': gate_at} if gate else None,
        'weekly_left': {'vendors': limits, 'at': limits_at} if limits else None,
        'provider_cooldowns': cooldowns,
        'vendors_today': {name: {'runs_today': heads_mod.runs_today(conn, name, cfg['owner_tz'], now),
                                 'daily_runs': v.get('daily_runs'), 'disabled': bool(v.get('disabled'))}
                          for name, v in (cfg.get('vendors') or {}).items()},
        'cards_by_state': counts,
        'slots': {'concurrency': int(cfg.get('concurrency', 2)), 'urgent_reserve': cfg['admission']['urgent_reserve'],
                  'used': len(heads), 'heads': heads, 'scripts': scripts, 'external': external,
                  'script_concurrency': cfg.get('script_concurrency', 4), 'gate_jobs': gate_jobs},
        'queue': {'length': len(order),
                  'next': [{'position': i + 1, 'card_id': c['id'], 'state': c['state'], 'title': c['title'],
                            'admission_class': api.admission_class(c, cfg), 'priority': c['priority'],
                            'created_at': c['created_at']} for i, c in enumerate(order[:QUEUE_SHOWN])]},
        'series': series(conn, now),
        'outbox': {'unpublished': unpublished, 'recent': recent},
    }


# ── search ───────────────────────────────────────────────────────────────────

PLACEMENTS = ('board', 'hidden')


def find(conn, *, states=None, card_class=None, parent=None, created_by=None, placement=None, query=None,
         followup_of=None, limit=20):
    """Cards newest first, filtered by state, class, parent epic, followup, creator, board placement (`board`: on
    the board or in its archive; `hidden`: a schedule's rule or a series run without a followup link) and a text
    `query` in the title or goal (case-insensitive substring). Cancelled cards only when `states` names them."""
    where, args = [], []
    if states:
        where.append(f"c.state IN ({','.join('?' * len(states))})")
        args += list(states)
    else:
        where.append("c.state != 'cancelled'")
    if card_class:
        where.append('c.card_class=?')
        args.append(card_class)
    if created_by:
        where.append('c.created_by=?')
        args.append(created_by)
    if parent:
        where.append("EXISTS (SELECT 1 FROM links p WHERE p.card_id=c.id AND p.kind='parent' AND p.target_id=?)")
        args.append(parent)
    if followup_of:
        where.append("EXISTS (SELECT 1 FROM links f WHERE f.card_id=c.id AND f.kind='followup' AND f.target_id=?)")
        args.append(followup_of)
    hidden = ("(c.kind='recurring' OR (c.card_class='series_run' AND NOT EXISTS "
              "(SELECT 1 FROM links f WHERE f.card_id=c.id AND f.kind='followup')))")
    if placement == 'hidden':
        where.append(hidden)
    elif placement == 'board':
        where.append(f'NOT {hidden}')
    elif placement is not None:
        raise ValueError(f'placement must be one of {", ".join(PLACEMENTS)}')
    if query:
        like = '%' + query.replace('\\', '\\\\').replace('%', '\\%').replace('_', '\\_') + '%'
        where.append("(c.title LIKE ? ESCAPE '\\' OR c.goal LIKE ? ESCAPE '\\')")
        args += [like, like]
    q = f"""SELECT c.id, c.state, c.wait_reason, c.round, c.title, c.kind, c.card_class, c.created_by, c.created_at,
                   c.updated_at, c.result,
                   (SELECT target_id FROM links p WHERE p.card_id=c.id AND p.kind='parent') AS parent,
                   (SELECT target_id FROM links f WHERE f.card_id=c.id AND f.kind='followup') AS followup_of
            FROM cards c WHERE {' AND '.join(where)} ORDER BY c.created_at DESC LIMIT ?"""
    return [dict(r, result=cut(r['result'], 500)) for r in conn.execute(q, args + [int(limit)])]
