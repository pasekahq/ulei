"""Heads: ladder lookup, weighted vendor pick, budget rule, CLI command lines, prompts."""
import datetime as dt
import glob
import json
import os
import pwd
import re
import shlex
import shutil
import subprocess
import time
from pathlib import Path
from zoneinfo import ZoneInfo

from . import api, store

VENDORS = ('claude', 'codex')
OTHER = {'claude': 'codex', 'codex': 'claude'}
# The launcher contract of ADR-0068 section 2.6 item 4.
TAG_ENV = 'HIVE_TAG'
TAG_REFUSAL_EXIT = 77
TAG_REFUSAL_LINE = re.compile(r'^tag refused: ([a-z][a-z0-9_]*): (.*)$', re.MULTILINE)
TAG_REFUSED = 'tag_refused'


# ── choice ────────────────────────────────────────────────────────────────

def executors(cfg):
    """Executor names a card may be assigned: configured in [vendors.*] and runnable by the dispatcher."""
    return [v for v in VENDORS if v in cfg['vendors']]


def head_for(cfg, step, vendor):
    h = cfg['ladder'][int(step)][vendor]
    return {'vendor': vendor, 'model': h['model'], 'effort': h['effort']}


def worker_head(cfg, card, vendor):
    """Use the stored pin on its vendor and the ladder for an alternate vendor."""
    if card['model'] is not None and vendor == card['assignee']:
        return {'vendor': vendor, 'model': card['model'], 'effort': card['effort']}
    return head_for(cfg, card['step'], vendor)


def pick_vendor(allowed, recent, weights):
    """Deterministic weighted pick: the vendor most under its weighted share wins.

    recent: vendors of the latest runs, newest last. A vendor that already took
    the last two runs is only chosen if it is the only one allowed. Not random,
    so the same head is never taken many times in a row by chance.
    """
    allowed = [v for v in VENDORS if v in allowed]
    if not allowed:
        return None
    if len(allowed) == 1:
        return allowed[0]
    streak = recent[-2:]
    pool = [v for v in allowed if not (len(streak) == 2 and streak[0] == streak[1] == v)] or allowed
    total = sum(weights.get(v, 1.0) for v in pool)

    def deficit(v):
        share = weights.get(v, 1.0) / total
        used = sum(1 for r in recent if r == v) / max(1, len(recent))
        last = max((i for i, r in enumerate(recent) if r == v), default=-1)
        return (used - share, last)  # most under-used first, then least recently used
    return min(pool, key=deficit)


def recent_vendors(conn, window):
    rows = conn.execute('SELECT vendor FROM runs WHERE role=? ORDER BY id DESC LIMIT ?', ('worker', window)).fetchall()
    return [r['vendor'] for r in reversed(rows)]


# ── budget ────────────────────────────────────────────────────────────────

def day_start(tz, now=None):
    """Start of the local day in zone `tz` (config `owner_tz`)."""
    d = dt.datetime.fromtimestamp(now or time.time(), ZoneInfo(tz))
    return d.replace(hour=0, minute=0, second=0, microsecond=0).timestamp()


def runs_today(conn, vendor, tz, now=None):
    return conn.execute('SELECT count(*) FROM runs WHERE vendor=? AND started_at>=? AND status!=?',
                        (vendor, day_start(tz, now), 'failed_launch')).fetchone()[0]


def parse_limits(data):
    out = {}
    for p in (data or {}).get('providers', []):
        for w in p.get('windows') or []:
            if w.get('window') == 'week' and isinstance(w.get('left_pct'), (int, float)):
                out[p.get('provider')] = float(w['left_pct'])
    return out


def fetch_limits(cfg):
    """Hook: weekly remaining % per vendor via the subscription-limits script (no model call)."""
    cmd = cfg.get('budget', {}).get('limits_command')
    if not cmd:
        return {}
    try:
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=40,
                           env=dict(os.environ, HOME=cfg['budget']['limits_home']))
        return parse_limits(json.loads(r.stdout)) if r.returncode == 0 else {}
    except (OSError, ValueError, subprocess.TimeoutExpired):
        return {}


def weekly_left(conn, cfg, fetcher=None, now=None):
    """Cached {vendor: left_pct}. Missing vendor = unknown = allowed (documented MVP default)."""
    now = now or time.time()
    cached, at = store.meta_get(conn, 'limits')
    if cached is not None and at and now - at < cfg.get('budget', {}).get('limits_ttl_s', 900):
        return cached
    value = (fetcher or fetch_limits)(cfg)
    with store.tx(conn):
        store.meta_set(conn, 'limits', value)
    return value


def no_tag_launcher(tag):
    return f'no tag launcher for tag {tag}'


def tag_capable(cfg, vendor):
    """The vendor is enabled and its configured binary is the tag-aware launcher."""
    vc = cfg['vendors'].get(vendor)
    return bool(vc) and not vc.get('disabled') and vc['tag_launcher']


def allowed_vendors(conn, cfg, step, limits, now=None, *, tag):
    """Vendors that may run a head for this step now, plus reasons for the others; for a tagged card only
    tag-capable vendors."""
    now = time.time() if now is None else now
    b = cfg.get('budget', {})
    low, low_max = b.get('low_weekly_left_pct', 20), b.get('low_max_step', 2)
    ok, why = [], {}
    for v in VENDORS:
        vc = cfg['vendors'].get(v)
        if not vc or vc.get('disabled'):
            why[v] = 'disabled'
            continue
        if tag is not None and not vc['tag_launcher']:
            why[v] = no_tag_launcher(tag)
            continue
        cooldown, _ = store.meta_get(conn, f'provider_cooldown:{v}')
        if cooldown and cooldown['until'] > now:
            why[v] = f"provider cooldown until {cooldown['until']:.0f} ({cooldown['failures']} failures)"
            continue
        if runs_today(conn, v, cfg['owner_tz'], now) >= vc.get('daily_runs', 10**9):
            why[v] = f"daily runs limit {vc.get('daily_runs')} reached"
            continue
        left = limits.get(v)
        if left is not None and left < low and int(step) > low_max:
            why[v] = f'weekly left {left:.0f}% < {low}%: only steps 1-{low_max}'
            continue
        ok.append(v)
    return ok, why


# ── command lines ───────────────────────────────────────────────────────────

def resolve_binary(candidates, path_env=None):
    for c in candidates:
        if ' ' in c:
            parts = shlex.split(c)
            if shutil.which(parts[0], path=path_env):
                return [shutil.which(parts[0], path=path_env)] + parts[1:]
            continue
        if any(ch in c for ch in '*?['):
            hits = [h for h in glob.glob(c) if os.access(h, os.X_OK)]
            if hits:
                return [max(hits, key=os.path.getmtime)]
            continue
        found = shutil.which(c, path=path_env) if '/' not in c else (c if os.access(c, os.X_OK) else None)
        if found:
            return [found]
    raise FileNotFoundError(f'no head binary among {candidates}')


CLAUDE_DENIED_TOOLS = {
    'browser': ('WebFetch', 'WebSearch'),
    'network': ('Bash', 'WebFetch', 'WebSearch'),
    'write': ('Bash', 'Edit', 'NotebookEdit', 'Write'),
}


def command(cfg, vendor, model, effort, workdir, run_dir, deny_tools, live=False):
    """Build a non-interactive vendor command with the card's tool policy."""
    deny_tools = api.normalize_deny_tools(deny_tools)
    vc = cfg['vendors'][vendor]
    base = resolve_binary(vc['binary'], path_env=head_env(cfg, vendor, None)['PATH'])
    if live and vendor == 'codex':
        search = 'disabled' if set(deny_tools) & {'browser', 'network'} else 'live'
        return base + ['-c', f'web_search="{search}"', 'app-server', '--listen', 'stdio://']
    if vendor == 'claude':
        argv = base + ['-p', '--model', model, '--effort', effort, '--output-format', 'json']
        if live:
            argv[-1] = 'stream-json'
            argv += ['--input-format', 'stream-json', '--verbose', '--replay-user-messages']
        if not deny_tools:
            return argv + ['--dangerously-skip-permissions']
        denied = tuple(dict.fromkeys(tool for policy in deny_tools for tool in CLAUDE_DENIED_TOOLS[policy]))
        return argv + ['--dangerously-skip-permissions', '--disallowed-tools', ','.join(denied),
                       '--strict-mcp-config']
    if not deny_tools:
        argv = base + ['--search', '--dangerously-bypass-approvals-and-sandbox', 'exec']
    else:
        sandbox = 'read-only' if 'write' in deny_tools else 'workspace-write'
        argv = base + ['--ask-for-approval', 'never', 'exec', '--sandbox', sandbox]
        if 'network' in deny_tools:
            argv += ['-c', 'sandbox_workspace_write.network_access=false']
    return argv + ['--skip-git-repo-check', '--json', '-m', model,
                   '-c', f'model_reasoning_effort="{effort}"', '-C', str(workdir),
                   '-o', str(Path(run_dir) / 'last-message.txt'), '-']


def head_env(cfg, vendor, tag):
    """The vendor's home and PATH, locale and terminal keys, the keys config `head_env` grants, the TLS and user-bus
    keys of this process and TAG_ENV holding a tagged card's tag; nothing else of the dispatcher's environment."""
    home = cfg['vendors'][vendor]['home']
    path = ':'.join([f'{home}/.local/bin', '/usr/local/bin', '/usr/bin', '/bin'])
    user = os.environ['USER'] if 'USER' in os.environ else pwd.getpwuid(os.getuid()).pw_name
    env = {'HOME': home, 'PATH': path, 'LANG': os.environ.get('LANG', 'C.UTF-8'),
           'USER': user, 'TERM': 'dumb', 'NO_COLOR': '1', **cfg['head_env']}
    for k in ('SSL_CERT_FILE', 'SSL_CERT_DIR', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'):
        if os.environ.get(k):
            env[k] = os.environ[k]
    if tag is not None:
        env[TAG_ENV] = tag
    return env


def parse_usage(vendor, log_text, run_dir):
    """Tokens + fallback report from the head's own output. Best effort."""
    tin = tout = None
    detail, text = None, None
    if vendor == 'claude':
        root_session = None
        for line in log_text.strip().splitlines():
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if not isinstance(d, dict) or d.get('parent_tool_use_id') is not None:
                continue
            session = d.get('session_id')
            if root_session is None and session:
                root_session = session
            if session and session != root_session:
                continue
            if d.get('type') == 'result':
                u = d.get('usage') or {}
                tin = (u.get('input_tokens') or 0) + (u.get('cache_read_input_tokens') or 0) + (u.get('cache_creation_input_tokens') or 0)
                tout = u.get('output_tokens')
                detail = {'usage': u, 'models': list((d.get('modelUsage') or {}).keys()), 'is_error': d.get('is_error'),
                          'num_turns': d.get('num_turns')}
                text = d.get('result')
    else:
        for line in log_text.splitlines():
            try:
                d = json.loads(line)
            except ValueError:
                continue
            if isinstance(d, dict) and d.get('type') == 'turn.completed':
                u = d.get('usage') or {}
                tin = (tin or 0) + (u.get('input_tokens') or 0)
                tout = (tout or 0) + (u.get('output_tokens') or 0) + (u.get('reasoning_output_tokens') or 0)
                detail = u
        last = Path(run_dir) / 'last-message.txt'
        if last.is_file():
            text = last.read_text(errors='replace')
    return tin, tout, detail, text


def provider_failure(status, code, log_text, tokens_out, stopped=False, is_error=False):
    """Classify failed head output for a same-round vendor retry."""
    if stopped or (status == 'exited' and code == 0 and not is_error):
        return None
    patterns = (
        ('http_429', r'\b(?:HTTP[/ ]?429|status(?:[ _]code)?["\s:]+429|too many requests)\b'),
        ('usage_limit', r'\b(?:usage limit|rate limit|quota (?:exceeded|exhausted)|limit (?:exceeded|reached)|hit your limit|out of credits)\b'),
        ('http_401', r'\b(?:HTTP[/ ]?401|status(?:[ _]code)?["\s:]+401|unauthorized|authentication failed)\b'),
        ('http_5xx', r'\b(?:HTTP[/ ]?5\d\d|status(?:[ _]code)?["\s:]+5\d\d|5\d\d internal server error|server error)\b'),
        ('network', r'\b(?:network error|fetch failed|connection (?:failed|refused|reset|timed out)|DNS error|ECONNRESET|ENOTFOUND|ETIMEDOUT)\b'),
    )
    for reason, pattern in patterns:
        if re.search(pattern, log_text, re.IGNORECASE):
            return reason
    return 'before_first_token' if not tokens_out else None


def tag_refusal(code, log_text, tag):
    """The run's error for a launcher refusal of `tag` (exit TAG_REFUSAL_EXIT with a TAG_REFUSAL_LINE), else None."""
    refusals = TAG_REFUSAL_LINE.findall(log_text)
    if code != TAG_REFUSAL_EXIT or not refusals:
        return None
    cls, message = refusals[-1]
    return f'{cls} for tag {tag}: {message.strip()}'


# ── workspace & prompts ──────────────────────────────────────────────────────

def workspace_context(cfg, card):
    """The configured workspace context (`texts.workspace_context`) with the card's tool policy."""
    deny_tools = card['deny_tools']
    if isinstance(deny_tools, str):
        deny_tools = json.loads(deny_tools)
    deny_tools = api.normalize_deny_tools(deny_tools)
    context = cfg['texts']['workspace_context']
    if not deny_tools:
        return context + '\n## Tool availability\n\nAll default tools are available, including browser, network, and write access.\n'
    denied = '\n'.join(f'- {tool} access is denied.' for tool in deny_tools)
    return context + f'\n## Tool availability\n\n{denied}\n'


def prepare_workspace(cfg, card, member=None, round_no=None):
    root = Path(cfg['workspaces']) / card['id']
    report_file = 'REPORT.md'
    if member is not None:
        root = root / 'members' / f'r{round_no}' / member['label']
        report_file = f"REPORT-{member['label']}.md"
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    for name in ('CLAUDE.md', 'AGENTS.md'):
        context = workspace_context(cfg, card).replace('REPORT.md', report_file)
        if member is not None:
            context += '\nWork independently. Do not read other member directories or reports in the parent card workspace.\n'
        (root / name).write_text(context)
    return root


def card_text(card):
    parts = [f"# Card {card['id']}: {card['title']}", '', '## Goal', card['goal'], '',
             '## Acceptance (checked mechanically after you finish; exit 0 = pass)', '```', card['acceptance'], '```']
    if 'card_class' in card.keys() and card['card_class'] == 'series_run':
        parts += ['', f"Result contract: card_class={card['card_class']}, owner_visible={bool(card['owner_visible'])}."]
    if card['body']:
        parts += ['', '## Details', card['body']]
    return '\n'.join(parts)


def worker_prompt(card, round_no, feedback, steers, stopped=None, report_file='REPORT.md', waited=None):
    """stopped: the round whose head the dispatcher stopped so that its unseen steers reach this one; waited: the round
    whose leftover processes the card waited on, which makes the feedback their report instead of a failure."""
    p = [card_text(card), '', f'Round {round_no}. Work in the current directory (the card workspace) unless the card names other paths.',
         f'When done, write {report_file} here (what you did + evidence).']
    if feedback:
        p += ['', f'## The processes round {waited} left running have ended' if waited else '## Previous round failed. Fix this:',
              feedback]
    if stopped:
        p += ['', f'## Round {stopped} was stopped for a clarification',
              'Its head was stopped mid-work so that you get the clarification below. Its files are still in this',
              'workspace: check them, keep what fits the clarification, and finish the card.']
    if steers:
        p += ['', '## Owner clarifications', 'Sent by the owner after the card was written. Your result must follow them.',
              'In REPORT.md quote each one verbatim and say how you applied it.']
        for s in steers:
            if not s['after_round']:
                when = 'before round 1'
            elif s['after_round'] == stopped:
                when = f'during round {stopped}'
            else:
                when = f"after the round {s['after_round']} report"
            p += ['', f"### Arrived {when} (from {s['by']})", s['text']]
    return '\n'.join(p)


def synthesis_prompt(card, round_no, reports, feedback):
    """Give the synthesis the brief and every member's fresh report and workspace."""
    prompt = [worker_prompt(card, round_no, feedback, []), '',
              '## Panel synthesis',
              'Read every member report and inspect their directories below. Produce the card deliverables',
              'in the current card workspace, where acceptance runs. Write your own synthesis in REPORT.md:',
              'agreements, disagreements, and which member said what. Name every member label explicitly.']
    for member in json.loads(card['panel']):
        label = member['label']
        prompt += ['', f'### Member {label}', f"Model: {member['model']}", f"Effort: {member['effort']}",
                   f'Directory: members/r{round_no}/{label}/', f'Report: REPORT-{label}.md', '',
                   reports[label]['report']]
    return '\n'.join(prompt)


def reviewer_prompt(card, round_no, report, gate):
    prompt = [
        'You are an independent REVIEWER. Do not redo the work; verify it against reality.',
        card_text(card), '',
        f'## Worker report (round {round_no})', report or '(no report)', '',
        '## Mechanical gate', f"exit {gate['exit_code']}: {(gate['output'] or '')[:2000]}", '',
        'Check the result (read files, run read-only commands). Then write REVIEW.md here whose',
        'FIRST line is exactly `VERDICT: PASS` or `VERDICT: FAIL`, followed by reasons.',
    ]
    if card['acceptance_kind'] == 'judge':
        prompt += [
            '', '## Persisted judge criteria', '```text', card['judge_criteria'], '```',
            'Issue `VERDICT: PASS` only when every persisted judge criterion is satisfied.',
            'Otherwise issue `VERDICT: FAIL` with actionable reasons.',
        ]
    prompt += ['FAIL only for concrete defects against the goal/acceptance; list what must change.']
    return '\n'.join(prompt)


def parse_verdict(text):
    for line in (text or '').splitlines():
        s = line.strip().strip('*`# ').upper()
        if s.startswith('VERDICT:'):
            v = s.split(':', 1)[1].strip()
            return 'PASS' if v.startswith('PASS') else 'FAIL' if v.startswith('FAIL') else None
    return None
