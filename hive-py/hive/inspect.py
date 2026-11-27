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

VERSION = 2
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
                'run_reaped', 'launch_error', 'steer_restart', 'disputed', 'dispute_resolved', 'wait_nudge')
PULL = [re.compile(r'\bgh\s+pr\s+(?:checks|view|merge|ready|diff)\s+(?:(?:-R|--repo)[= ]?\S+\s+)?#?(\d+)\b'),
        re.compile(r'github\.com/([\w.-]+/[\w.-]+)/pull/(\d+)')]
RUN_WATCH = re.compile(r'\bgh\s+run\s+(?:watch|view)\s+(?:(?:-R|--repo)[= ]?\S+\s+)?(\d+)\b')
# What a vendor CLI puts around the command a head asked for (sourcing its shell snapshot, unaliasing, a `case` on the
# shell version; after it, stdin from /dev/null for a background command and saving the working directory): the
# snapshot shows the command inside `eval '…'`.
PREAMBLE = re.compile(r"^/bin/(?:ba|z)?sh -c (?:source \S+ |\{ ?case |[^\n]{0,400}?shell-snapshots)[\s\S]{0,8000}?&& eval '")
POSTAMBLE = re.compile(r"'(?: < /dev/null)?(?: \\?&& pwd -P >\| \S+)?(?: ?\})?\s*$")
ACTIONS = 10              # recent actions of a head read from the tail of its log
ACTION_CHARS = 300        # one action's text
LOG_TAIL = 512 * 1024     # the bytes of a head's log read for its recent actions
MAX_REPOS = 4             # git repositories of a workspace described in a snapshot
GIT_TIMEOUT_S = 5
REPO_FLAG = re.compile(r'(?:--repo[= ]|-R\s+)([\w.-]+/[\w.-]+)')
# A unit name as a word of its own: not part of a path (`/srv/name/x`) or of an `owner/name` repository.
UNIT_TOKEN = re.compile(r'(?<![\w@.:/-])[A-Za-z0-9@_.:-]+(?![\w@.:/-])')


# ── secrets ───────────────────────────────────────────────────────────────────
# A command line or a head's action can carry a credential (`curl -H "Authorization: Bearer …"`, `GH_TOKEN=… gh`,
# `psql postgres://user:pass@…`); `redact` replaces each with MASK before a snapshot shows the text.
MASK = '[REDACTED]'
SECRET_TEXT = [
    # a private key block, whole
    (re.compile(r'-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)'), MASK),
    # an Authorization (or Proxy-Authorization) header: its scheme kept, its credential masked
    (re.compile(r'(?i)(\b(?:proxy-)?authorization\s*[:=]\s*)(?:(bearer|basic|token|digest)\s+)?[^\s\'",;]+'),
     lambda m: m.group(1) + (m.group(2) + ' ' if m.group(2) else '') + MASK),
    (re.compile(r'(?i)\b(bearer)\s+[A-Za-z0-9._~+/=-]{8,}'), lambda m: m.group(1) + ' ' + MASK),
    # credentials in a URL: scheme://user:pass@host, scheme://token@host
    (re.compile(r'(?i)\b([a-z][a-z0-9+.-]*://)[^/\s:@\'"]+:[^/\s@\'"]*@'), r'\1' + MASK + '@'),
    (re.compile(r'(?i)\b((?:https?|git|ssh)://)[A-Za-z0-9_-]{20,}@'), r'\1' + MASK + '@'),
    # NAME=value where the name speaks of a secret: GH_TOKEN=, API_KEY=, AWS_SECRET_ACCESS_KEY=, PGPASSWORD=
    (re.compile(r'(?i)\b([A-Z0-9_]*(?:TOKEN|SECRET|PASSWORD|PASSWD|PASSPHRASE|_KEY|APIKEY|CREDENTIALS?|_PAT|_AUTH)'
                r'[A-Z0-9_]*)=("[^"]*"|\'[^\']*\'|[^\s\'";&|]+)'), r'\1=' + MASK),
    (re.compile(r'(?i)(?<![\w-])(key|secret|password|token)=("[^"]*"|\'[^\']*\'|[^\s\'";&|]+)'), r'\1=' + MASK),
    # "password": "…" in JSON, password: … in YAML
    (re.compile(r'(?i)(["\']?\b(?:password|passwd|secret|token|api[_-]?key|access[_-]?key|client[_-]?secret|'
                r'private[_-]?key)["\']?\s*:\s*)("[^"]*"|\'[^\']*\'|[^\s,\'"}]+)'), r'\1' + MASK),
    # --token X, --password=X, --api-key X, -p'X' of mysql
    (re.compile(r'(?i)(--(?:[a-z-]*-)?(?:token|password|passwd|secret|api-key|access-key|auth)(?:=|\s+))'
                r'("[^"]*"|\'[^\']*\'|[^\s\'"]+)'), r'\1' + MASK),
    # token formats that are secrets on their own, wherever they stand
    (re.compile(r'\b(?:gh[opusr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|glpat-[A-Za-z0-9_-]{20,}|'
                r'sk-(?:ant-|proj-)?[A-Za-z0-9_-]{20,}|xox[abprs]-[A-Za-z0-9-]{10,}|xapp-[A-Za-z0-9-]{10,}|'
                r'(?:AKIA|ASIA)[0-9A-Z]{16}|AIza[0-9A-Za-z_-]{35}|ya29\.[0-9A-Za-z_-]{20,}|'
                r'npm_[A-Za-z0-9]{36}|pypi-[A-Za-z0-9_-]{50,}|hf_[A-Za-z0-9]{30,}|'
                r'eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,})'), MASK),
    # a Telegram bot token: 123456789:AA…
    (re.compile(r'(?<![0-9])\d{8,10}:AA[A-Za-z0-9_-]{30,}'), MASK),
]


def redact(text):
    """`text` with the credentials SECRET_TEXT knows replaced by MASK; None stays None."""
    if not text:
        return text
    for pattern, repl in SECRET_TEXT:
        text = pattern.sub(repl, text)
    return text


def unwrap(cmdline):
    """The command a head asked for, without the vendor CLI's shell wrapper around it."""
    m = PREAMBLE.match(cmdline)
    if not m:
        return cmdline
    inner = POSTAMBLE.sub('', cmdline[m.end():], count=1)
    return inner.replace("'\"'\"'", "'").replace("'\\''", "'")


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
        shown = redact(unwrap(full))
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

    def branch_pull(self, branch, repo, cwd):
        """The latest pull request (any state) whose head is `branch`, through `gh pr list`; None when there is none."""
        found = gh_json(self, ['pr', 'list', '--repo', repo, '--head', branch, '--state', 'all', '--limit', '1',
                               '--json', 'number,url,state,title,updatedAt,mergedAt'], cwd)
        if isinstance(found, dict):
            return found
        return found[0] if found else None


def gh_json(probe, argv, cwd):
    if not probe.gh:
        return {'error': 'no gh command configured'}
    try:
        out = subprocess.run([*probe.gh, *argv], cwd=cwd if cwd and os.path.isdir(cwd) else None, capture_output=True,
                             text=True, timeout=GH_TIMEOUT_S)
    except (OSError, subprocess.TimeoutExpired) as e:
        return {'error': f'gh: {e.__class__.__name__}: {e}'[:300]}
    if out.returncode != 0:
        return {'error': redact(f'gh exit {out.returncode}: {out.stderr.strip()[:300]}')}
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
        entry.update(recent_actions(r['run_dir'], r['vendor']))
        heads.append(entry)
    jobs = [dict(j) for j in conn.execute('SELECT version, mode, started_at FROM gate_jobs WHERE card_id=?',
                                          (card['id'],))]
    for j in jobs:
        j['running_s'] = round(now - j['started_at'], 1)
    return {'heads': heads, 'gate_jobs': jobs, 'repos': repos(card['workdir'], probe)}


# ── what a head did last ──────────────────────────────────────────────────────

def log_tail(path, size=LOG_TAIL):
    """The last `size` bytes of the file as lines, the first (cut) line dropped; [] when it cannot be read."""
    try:
        with open(path, 'rb') as f:
            f.seek(0, os.SEEK_END)
            end = f.tell()
            f.seek(max(0, end - size))
            raw = f.read(size)
    except OSError:
        return []
    lines = raw.decode(errors='replace').splitlines()
    return lines[1:] if end > size and lines else lines


def iso_ts(value):
    if isinstance(value, (int, float)):
        return value / 1000 if value > 1e11 else float(value)
    if not isinstance(value, str):
        return None
    from datetime import datetime
    try:
        return datetime.fromisoformat(value.replace('Z', '+00:00')).timestamp()
    except ValueError:
        return None


def tool_text(name, args):
    """One line for a tool call: the command of a shell, the path of a file tool, else its arguments."""
    if not isinstance(args, dict):
        return f'{name} {args}'
    for key in ('command', 'cmd', 'file_path', 'path', 'pattern', 'url', 'query', 'description', 'prompt'):
        if isinstance(args.get(key), str):
            value = args[key] if key != 'command' else unwrap(args[key])
            return f'{name}: {value}'
        if isinstance(args.get(key), list):
            return f'{name}: {" ".join(map(str, args[key]))}'
    return f'{name} {json.dumps(args, ensure_ascii=False)}'


def claude_actions(d):
    """[(kind, text)] of one line of a Claude stream: its tool calls and texts; tool results are left out."""
    if d.get('type') != 'assistant' or d.get('parent_tool_use_id') is not None:
        return []
    out = []
    for c in (d.get('message') or {}).get('content') or []:
        if not isinstance(c, dict):
            continue
        if c.get('type') == 'tool_use':
            out.append(('tool', tool_text(c.get('name') or '?', c.get('input'))))
        elif c.get('type') == 'text' and (c.get('text') or '').strip():
            out.append(('text', c['text']))
    return out


CODEX_ITEMS = {'commandExecution': 'tool', 'command_execution': 'tool', 'mcpToolCall': 'tool', 'mcp_tool_call': 'tool',
               'fileChange': 'tool', 'file_change': 'tool', 'webSearch': 'tool', 'web_search': 'tool',
               'agentMessage': 'text', 'agent_message': 'text'}


def codex_actions(d):
    """[(kind, text)] of one line of a Codex stream (`exec --json` or the app server): its started items."""
    kind = d.get('type') or d.get('method')
    if kind not in ('item.started', 'item/started', 'item.completed', 'item/completed'):
        return []
    item = d.get('item') or (d.get('params') or {}).get('item') or {}
    what = CODEX_ITEMS.get(item.get('type'))
    if what is None:
        return []
    # A tool shows when it starts, a message when it is complete.
    if (what == 'tool') != kind.endswith('started'):
        return []
    if what == 'text':
        return [('text', item.get('text') or '')]
    t = item.get('type')
    if t in ('commandExecution', 'command_execution'):
        return [('tool', tool_text('shell', {'command': item.get('command')}))]
    if t in ('fileChange', 'file_change'):
        return [('tool', 'edit: ' + ', '.join(ch.get('path', '?') for ch in item.get('changes') or []))]
    if t in ('webSearch', 'web_search'):
        return [('tool', f"search: {item.get('query')}")]
    return [('tool', tool_text(f"{item.get('server')}.{item.get('tool')}", item.get('arguments')))]


def recent_actions(run_dir, vendor, n=ACTIONS):
    """The head's last `n` actions read from the tail of its log, oldest first: {at, kind tool|text, text}, every text
    cut and redacted; with the log's size and last write. A snapshot of the file, not a stream."""
    if not run_dir:
        return {'actions': [], 'log': None}
    path = Path(run_dir) / 'head.log'
    try:
        st = path.stat()
    except OSError:
        return {'actions': [], 'log': None}
    found = []
    for line in log_tail(path):
        if not line.startswith('{'):
            continue
        try:
            d = json.loads(line)
        except ValueError:
            continue
        if not isinstance(d, dict):
            continue
        acts = claude_actions(d) if vendor == 'claude' or 'session_id' in d else codex_actions(d)
        at = iso_ts(d.get('timestamp') or d.get('emittedAtMs') or d.get('ts'))
        for kind, text in acts:
            found.append({'at': at, 'kind': kind, 'text': redact(cut(' '.join(text.split()), ACTION_CHARS))})
    return {'actions': found[-n:], 'log': {'size': st.st_size, 'updated_at': st.st_mtime}}


# ── the workspace's repositories ──────────────────────────────────────────────

def git(cwd, *argv):
    try:
        out = subprocess.run(['git', '-C', str(cwd), *argv], capture_output=True, text=True, timeout=GIT_TIMEOUT_S,
                             env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0', 'GIT_TERMINAL_PROMPT': '0'})
    except (OSError, subprocess.TimeoutExpired):
        return None
    return out.stdout.strip() if out.returncode == 0 else None


GITHUB_REMOTE = re.compile(r'github\.com[:/]([\w.-]+/[\w.-]+?)(?:\.git)?/?$')


def repos(workdir, probe):
    """The git repositories of the card's workspace (the workspace itself and its top-level directories): branch,
    last commit, how far it is ahead of its upstream, dirty or not, and the latest pull request of the branch — what
    the head last committed and pushed, whatever it waits on now."""
    if not workdir or not os.path.isdir(workdir):
        return []
    root = Path(workdir)
    subdirs = sorted(d for d in root.iterdir() if d.is_dir() and not d.is_symlink() and (d / '.git').exists())
    candidates = ([root] if (root / '.git').exists() else []) + subdirs
    out = []
    for d in candidates[:MAX_REPOS]:
        head = git(d, 'log', '-1', '--format=%H%x1f%ct%x1f%an%x1f%s')
        if head is None:
            continue
        sha, ct, author, subject = (head.split('\x1f') + ['', '', '', ''])[:4]
        branch = git(d, 'rev-parse', '--abbrev-ref', 'HEAD')
        upstream = git(d, 'rev-parse', '--abbrev-ref', '--symbolic-full-name', '@{upstream}')
        ahead = git(d, 'rev-list', '--count', '@{upstream}..HEAD') if upstream else None
        status = git(d, 'status', '--porcelain', '--untracked-files=no')
        remote = git(d, 'remote', 'get-url', 'origin')
        m = GITHUB_REMOTE.search(remote or '')
        entry = {'path': str(d.relative_to(root)) if d != root else '.', 'branch': branch, 'upstream': upstream,
                 'ahead': int(ahead) if ahead and ahead.isdigit() else None, 'dirty': bool(status),
                 'remote': redact(remote), 'repo': m.group(1) if m else None,
                 'last_commit': {'sha': sha[:12], 'at': float(ct) if ct.isdigit() else None, 'author': author,
                                 'subject': redact(cut(subject, ACTION_CHARS))},
                 'pull_request': None}
        if m and branch and branch != 'HEAD':
            pr = probe.branch_pull(branch, m.group(1), str(d))
            entry['pull_request'] = pr if pr is None or 'error' in pr else {
                k: pr.get(k) for k in ('number', 'url', 'state', 'title', 'updatedAt', 'mergedAt')}
        out.append(entry)
    return out


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
    """PASS, FAIL or DISPUTED (no failure: the card's author decides) of a round and by whom, None while it has neither
    a gate, a judge, a dispute nor a reviewer verdict."""
    judge, gate = entry['judge'], entry['gate']
    reviews = [r for r in entry['runs'] if r['role'] == 'reviewer' and r['verdict']]
    disputed = [e for e in entry['events'] if e['kind'] == 'disputed']
    if disputed:
        return {'result': 'DISPUTED', 'by': disputed[-1]['data'].get('by'), 'detail': 'the card author decides'}
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
    card = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    out = []
    for r in conn.execute('SELECT event_id, kind, payload, created_at, published_at, error FROM outbox WHERE card_id=? '
                          'ORDER BY created_at', (card_id,)):
        local = quiet_result(card, r['kind'], loads(r['payload']) or {})
        row = {k: r[k] for k in ('event_id', 'kind', 'created_at', 'published_at', 'error')}
        row['destination'] = 'local' if local else 'owner-inbox'
        row['inbox'] = inbox_status(adapter, r['event_id']) if r['published_at'] and not local else None
        out.append(row)
    return out


def quiet_result(card, kind, payload):
    """True for the done result of a quiet series run (series_run, not owner_visible, no [NEWS], not stopped): the
    dispatcher's publish marks it published and keeps it local, so no inbox row is expected (Dispatcher.publish)."""
    result = payload.get('result') or {}
    return bool(card is not None and card['card_class'] == 'series_run' and not card['owner_visible']
                and kind == 'task_result' and payload.get('state') == 'done' and not payload.get('loop_stopped')
                and result.get('news') is not True and result.get('series_stopped') is not True
                and not card['series_stopped'])


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
    """The active head runs (any card, board or not: series runs, reviewers, synthesis), the scripts and the external
    jobs, each marked `urgent` when claimed on the urgent lane."""
    rows = conn.execute(f"""SELECT r.id AS run_id, r.card_id, r.role, r.vendor, r.model, r.effort, r.unit, r.started_at,
                                  c.title, c.card_class, c.kind, c.executor, c.script, {api.URGENT_RUN} AS urgent
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
    # A head launched after the note: the note is about an earlier wait (a store written before the dispatcher
    # cleared notes at launch keeps it), not why the card waits now.
    last_start = conn.execute('SELECT max(started_at) FROM runs WHERE card_id=?', (card['id'],)).fetchone()[0]
    if budget and last_start is not None and last_start >= at:
        budget = None
    waits = card['state'] in ('queued', 'review', 'created') or column == 'waiting'
    # A note older than the config file came from a dispatcher that ran the old limits: `stale`.
    mtime = config_mtime(cfg)
    out['budget_wait'] = ({'vendors': budget, 'at': at, 'age_s': round(now - at, 1),
                           'stale': bool(mtime and at < mtime)}
                          if budget and waits and not store.is_final(card) else None)
    return out


def slots(cfg, card, heads):
    """The slots the card waits for: an urgent card the urgent lane's URGENT_MAX cards, any other the concurrency
    head runs outside it."""
    urgent = api.admission_class(card, cfg) == 'urgent'
    ceiling = int(cfg.get('concurrency', 2))
    lane = [h for h in heads if bool(h['urgent']) == urgent]
    limit, used = (api.URGENT_MAX, len({h['card_id'] for h in lane})) if urgent else (ceiling, len(lane))
    return {'concurrency': ceiling, 'urgent_max': api.URGENT_MAX, 'limit_for_this_card': limit, 'used': used,
            'free': max(0, limit - used),
            'holders': [{k: h[k] for k in ('run_id', 'card_id', 'role', 'card_class', 'started_at', 'title')}
                        for h in lane]}


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
    """{path, size, offset, text, truncated} of a file, or {path, entries} of a directory, of the card's workspace;
    the text with the credentials `redact` knows masked (a head's log can hold one)."""
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
    return {'path': rel, 'size': size, 'offset': offset, 'text': redact(raw.decode(errors='replace')),
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
    recent = []
    for r in conn.execute('SELECT event_id, card_id, kind, payload, created_at, published_at, error FROM outbox '
                          'ORDER BY rowid DESC LIMIT 20').fetchall():
        c = conn.execute('SELECT * FROM cards WHERE id=?', (r['card_id'],)).fetchone()
        local = quiet_result(c, r['kind'], loads(r['payload']) or {})
        row = {k: r[k] for k in ('event_id', 'card_id', 'kind', 'created_at', 'published_at', 'error')}
        row['destination'] = 'local' if local else 'owner-inbox'
        row['inbox'] = inbox_status(adapter, r['event_id']) if r['published_at'] and not local else None
        recent.append(row)
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
        'slots': {'concurrency': int(cfg.get('concurrency', 2)), 'urgent_max': api.URGENT_MAX,
                  'used': sum(not h['urgent'] for h in heads),
                  'urgent_used': len({h['card_id'] for h in heads + scripts if h['urgent']}),
                  'heads': heads, 'scripts': scripts, 'external': external,
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
