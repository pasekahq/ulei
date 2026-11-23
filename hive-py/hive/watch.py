"""Watch tick: a Hive script step that runs a monitor's collector through the watch gate and starts a model child
only on a gate wake, and the child's acceptance check that routes its delivery.

The child carries the handoff of one gate id. A tick acknowledges that gate only after the child is done, its gate
passed and its task_result outbox row is published without error; any other child end fails the handoff, so its
items stay owed to the next wake.
"""
import argparse
import contextlib
import json
import math
import os
import re
import secrets
import shlex
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from datetime import time as clock
from pathlib import Path

from . import api, heads, script, store
from . import watch_gate as wg

REPO = Path(__file__).resolve().parents[1]
COLLECTOR_KEY = 'src:collector'
SILENT = '[SILENT]'
HOLD_S = 10 * 365 * 86400
SECRET_NAME = re.compile(r'TOKEN|SECRET|PASSWORD|PASSWD|API_?KEY|PRIVATE|CREDENTIAL|COOKIE|AUTH', re.I)
CONFIG_KEYS = {'job', 'collector', 'prefixes', 'policy', 'checkpoint', 'child'}
COLLECTOR_KEYS = {'script', 'args', 'timeout'}
POLICY_KEYS = {'news_destination', 'lease', 'quiet', 'wake_on_cleared', 'error_grace'}
QUIET_KEYS = {'start', 'end', 'tz', 'destinations'}
CHILD_KEYS = {'step', 'title', 'prompt', 'prompt_path'}
SIGNAL_KEYS = {'key', 'class', 'fingerprint', 'wake_at', 'covers', 'bypass'}


class ConfigError(ValueError):
    pass


class CheckError(ValueError):
    pass


@dataclass(frozen=True)
class Job:
    path: Path
    job: str
    collector: tuple[str, ...]
    collector_timeout: float
    prefixes: tuple[str, ...]
    policy: wg.Policy
    checkpoint: Path
    step: int
    title: str
    prompt: str

    @property
    def source(self):
        return f'cron:{self.job}:conditional'

    def idem_key(self, gate):
        return f'watch:{self.job}:{gate}'


def load(path, tz):
    """Parse and validate one job config; `tz` is the zone of a quiet window that names none."""
    path = Path(path)
    data = json.loads(path.read_text(encoding='utf-8'))
    _keys(data, CONFIG_KEYS, CONFIG_KEYS, 'config')
    job = data['job']
    if not isinstance(job, str) or not re.fullmatch(r'[A-Za-z0-9_.-]+', job):
        raise ConfigError('job must be a non-empty id of letters, digits, dot, dash or underscore')
    collector = data['collector']
    _keys(collector, COLLECTOR_KEYS, {'script', 'timeout'}, 'collector')
    args = collector.get('args', [])
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        raise ConfigError('collector args must be a list of strings')
    timeout = _positive(collector['timeout'], 'collector timeout')
    prefixes = data['prefixes']
    if not isinstance(prefixes, list) or not prefixes or not all(isinstance(p, str) and p for p in prefixes):
        raise ConfigError('prefixes must be a non-empty list of key prefixes')
    if COLLECTOR_KEY.startswith(tuple(prefixes)):
        raise ConfigError(f'prefixes must not cover the reserved key {COLLECTOR_KEY}')
    child = data['child']
    _keys(child, CHILD_KEYS, {'step', 'title'}, 'child')
    step = child['step']
    if isinstance(step, bool) or step not in range(1, 6):
        raise ConfigError('child step must be 1..5')
    if not isinstance(child['title'], str) or not child['title'].strip():
        raise ConfigError('child title must be non-blank text')
    if ('prompt' in child) == ('prompt_path' in child):
        raise ConfigError('child takes exactly one of prompt and prompt_path')
    prompt = child['prompt'] if 'prompt' in child else _absolute(child['prompt_path'], 'prompt_path').read_text()
    if not isinstance(prompt, str) or not prompt.strip():
        raise ConfigError('child prompt must be non-blank text')
    policy = _policy(data['policy'], tz)
    try:
        argv = script.command(_absolute(collector['script'], 'collector script')) + args
    except ValueError as e:
        raise ConfigError(str(e)) from None
    return Job(path.resolve(), job, tuple(argv), timeout, tuple(prefixes), policy,
               _absolute(data['checkpoint'], 'checkpoint'), step, child['title'].strip(), prompt)


def tick(job, cfg, conn, card_id, workdir, now=None):
    """One tick under the checkpoint lock: settle the pending handoff, collect, decide, then hand off or record.
    Returns a summary for the tick report."""
    now = time.time() if now is None else now
    floor = cfg['timeouts'].get(job.step, 3600) + cfg.get('gate_timeout_s', 300)
    if job.policy.lease <= floor:
        raise ConfigError(f'lease {job.policy.lease}s must exceed the child run and acceptance timeouts ({floor}s)')
    workdir = Path(workdir)
    with wg.locked(job.checkpoint) as tx:
        settled = _settle(job, cfg, conn, card_id, tx, now)
        signals = collect(job, cfg, card_id, workdir)
        decision = wg.evaluate(tx.checkpoint, signals, now, job.policy)
        summary = {'job': job.job, 'settled': settled, 'action': decision.action,
                   'items': [[i.key, i.change, i.cls, i.held] for i in decision.items]}
        if decision.action == 'wake' and tx.checkpoint.pending is None:
            gate = secrets.token_hex(8)
            tx.commit(wg.begin(tx.checkpoint, decision, gate, now, job.policy))
            summary.update(gate=gate, child=_hand_off(job, cfg, conn, card_id, tx.checkpoint.pending))
            return summary
        if decision.action == 'wake':
            summary['action'] = 'wait-active'
        if decision.items or decision.absorbed or decision.suspects != tx.checkpoint.suspects:
            tx.commit(wg.observe(tx.checkpoint, decision))
        return summary


def collect(job, cfg, card_id, workdir):
    """Run the collector; a nonzero exit, timeout or invalid output becomes one source_error covering every prefix."""
    env = script.environment(cfg, card_id)
    process = subprocess.Popen(list(job.collector), cwd=workdir, env=env, stdin=subprocess.DEVNULL,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        out, err = process.communicate(timeout=job.collector_timeout)
        failure = None if process.returncode == 0 else f'collector exited with status {process.returncode}'
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        out, err = process.communicate()
        failure = f'collector timed out after {job.collector_timeout:g}s'
    wg._atomic_write(workdir / 'collector.stderr.log', redact(err.decode('utf-8', 'replace'), env,
                                                              Path(cfg['script_env_file'])))
    if failure is None:
        try:
            return parse(out, job.prefixes)
        except (ValueError, TypeError, KeyError):
            failure = 'collector output is not a valid signal list'
    return [wg.Signal(COLLECTOR_KEY, 'source_error', failure, covers=job.prefixes)]


def parse(out, prefixes):
    """Signals from collector stdout `{"signals": [...]}`; every key must lie under a configured prefix."""
    data = json.loads(out)
    if not isinstance(data, dict) or set(data) != {'signals'} or not isinstance(data['signals'], list):
        raise ValueError('collector output must be {"signals": [...]}')
    signals = []
    for raw in data['signals']:
        _keys(raw, SIGNAL_KEYS, {'key', 'class', 'fingerprint'}, 'signal')
        if not isinstance(raw['key'], str) or not raw['key'].startswith(tuple(prefixes)):
            raise ValueError(f"signal key {raw['key']!r} is outside the configured prefixes")
        covers = raw.get('covers', [])
        if not isinstance(covers, list):
            raise ValueError('signal covers must be a list')
        signals.append(wg.Signal(raw['key'], raw['class'], raw['fingerprint'], raw.get('wake_at'), tuple(covers),
                                 raw.get('bypass', False)))
    wg._index(signals)
    return signals


def redact(text, env, env_file):
    """Replace the values of `env_file` and secret-named environment values in `text`."""
    values = set(script.read_env(env_file).values()) if env_file.is_file() else set()
    values.update(v for k, v in env.items() if SECRET_NAME.search(k))
    for value in sorted((v for v in values if len(v) >= 4), key=len, reverse=True):
        text = text.replace(value, '[REDACTED]')
    return text


def render(handoff):
    """Human-readable diff of a handoff, the run's script output."""
    lines = [f'Watch handoff {handoff.gate}: {len(handoff.items)} change(s)']
    for item in handoff.items:
        state = item.after.fingerprint if item.after else 'cleared'
        was = f' (was: {item.before.fingerprint})' if item.before and item.after else ''
        again = ' [owed again]' if item.redelivered else ''
        lines.append(f'- {item.change} {item.cls} {item.key}: {state}{was}{again}')
    return '\n'.join(lines) + '\n'


def check(job, conn, card_id, workdir):
    """Validate the child's result files and switch its delivery from owner-inbox only after every check passed.
    Returns the destination."""
    workdir = Path(workdir).resolve()
    card = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    if card is None or card['source'] != job.source or card['state'] != 'check':
        raise CheckError('card is not a watch child of this job in check')
    if card['workdir'] is None or Path(card['workdir']).resolve() != workdir:
        raise CheckError('check runs outside the child workdir')
    _route(conn, card, card['deliver'], 'owner-inbox')
    card = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
    pending = wg.read(job.checkpoint).pending
    if pending is None or card['idem_key'] != job.idem_key(pending.gate):
        raise CheckError('the child does not hold the pending handoff')
    result = _result(job, workdir)
    if result['message'].strip() == SILENT:
        destination = 'local'
    elif (job.policy.news_destination == 'telegram' and result['news']
          and any(i.cls == 'news' and i.destination == 'telegram' for i in pending.items)):
        destination = 'telegram'
    else:
        destination = 'owner-inbox'
    with store.tx(conn):
        conn.execute('UPDATE cards SET owner_visible=? WHERE id=?', (int(result['news']), card_id))
    return _route(conn, card, 'owner-inbox', destination)


def _result(job, workdir):
    try:
        data = json.loads((workdir / 'result.json').read_text())
    except (OSError, ValueError) as e:
        raise CheckError(f'result.json unreadable: {type(e).__name__}') from None
    if not isinstance(data, dict) or data.get('job_id') != job.job:
        raise CheckError(f'result.json job_id must be {job.job!r}')
    if data.get('status') != 'ok':
        raise CheckError(f"result.json status={data.get('status')!r}; see EVIDENCE.md")
    if not isinstance(data.get('message'), str) or not data['message'].strip():
        raise CheckError('result.json message must be non-blank text')
    if not isinstance(data.get('evidence'), list) or not data['evidence']:
        raise CheckError('result.json evidence must be a non-empty list')
    if not isinstance(data.get('news'), bool):
        raise CheckError('result.json news must be true or false')
    if data['news'] and data['message'].strip() == SILENT:
        raise CheckError(f'a {SILENT} result declares no news')
    for name in ('REPORT.md', 'EVIDENCE.md'):
        if not (workdir / name).is_file() or not (workdir / name).read_text().strip():
            raise CheckError(f'{name} is required')
    return data


def _route(conn, card, expected, destination):
    """Compare-and-set the delivery of a child in check without bumping its version, so the running gate keeps it.
    A steered child never becomes telegram: a steer can reopen it after the check, and a failed later round would
    publish by the kept delivery. Returns the destination set."""
    with store.tx(conn):
        if destination == 'telegram' and conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='steer'",
                                                      (card['id'],)).fetchone():
            destination = 'owner-inbox'
        cur = conn.execute("""UPDATE cards SET deliver=?, updated_at=? WHERE id=? AND state='check' AND version=?
                              AND deliver=?""", (destination, time.time(), card['id'], card['version'], expected))
        if cur.rowcount != 1:
            raise CheckError('child changed during the check')
        if expected != destination:
            store.event(conn, card['id'], 'watch_delivery', {'from': expected, 'to': destination}, actor='watch')
    return destination


def _settle(job, cfg, conn, card_id, tx, now):
    """Resolve the pending handoff from its child's real outcome; returns what happened, or None without one."""
    pending = tx.checkpoint.pending
    if pending is None:
        return None
    child = _reclaim(conn, job.idem_key(pending.gate))
    expired = now >= pending.lease_until
    if child is None:
        if expired:
            tx.commit(wg.fail(tx.checkpoint, pending.gate))
            return {'gate': pending.gate, 'outcome': 'lost'}
        return {'gate': pending.gate, 'outcome': 'staged', 'child': _hand_off(job, cfg, conn, card_id, pending)}
    outcome = _outcome(conn, child, expired)
    if outcome in ('abandoned', 'starved'):
        outcome = _cancel(cfg, conn, child, outcome, expired)
    if outcome == 'delivered':
        tx.commit(wg.ack(tx.checkpoint, pending.gate))
    elif outcome in ('failed', 'abandoned', 'starved'):
        tx.commit(wg.fail(tx.checkpoint, pending.gate))
    elif child['state'] == 'created' and child['not_before'] is not None:
        _hand_off(job, cfg, conn, card_id, pending)
    return {'gate': pending.gate, 'outcome': outcome, 'child': child['id']}


def _cancel(cfg, conn, child, outcome, expired):
    """Cancel an abandoned or starved child; returns the outcome to settle with.

    The dispatcher may pass the child's gate between `_outcome` and the cancel (lease expired while the
    finished worker waited for its check). Then cancel refuses the done card, and the child is judged
    again as the done card it now is instead of failing the tick.
    """
    from .dispatcher import Dispatcher
    reason = ('watch tick: child not started / starved before lease expired; its items stay owed'
              if outcome == 'starved' else 'watch tick: child attempt failed; its items stay owed')
    try:
        Dispatcher(cfg, conn).cancel(child['id'], reason, actor='secretary')
    except api.CardError:
        fresh = api.card_row(conn, child['id'])
        if fresh['state'] != 'done':
            raise
        return _outcome(conn, fresh, expired)
    return outcome


def _reclaim(conn, idem_key):
    """The handoff's child, first routed back to owner-inbox when a steer reopened it after a telegram check."""
    with store.tx(conn):
        child = conn.execute('SELECT * FROM cards WHERE idem_key=?', (idem_key,)).fetchone()
        if child is None or child['deliver'] != 'telegram' or child['state'] in ('check', 'done'):
            return child
        conn.execute("UPDATE cards SET deliver='owner-inbox', updated_at=? WHERE id=? AND deliver='telegram'",
                     (time.time(), child['id']))
        store.event(conn, child['id'], 'watch_delivery', {'from': 'telegram', 'to': 'owner-inbox'}, actor='watch')
        return conn.execute('SELECT * FROM cards WHERE id=?', (child['id'],)).fetchone()


def _outcome(conn, child, expired):
    """delivered, delivering (done, outbox not yet published), failed (final), abandoned (to cancel) or active."""
    if child['state'] == 'done':
        gate = conn.execute('SELECT passed FROM gates WHERE card_id=? ORDER BY id DESC LIMIT 1',
                            (child['id'],)).fetchone()
        if gate is None or not gate['passed']:
            return 'failed'
        row = conn.execute("SELECT published_at, error FROM outbox WHERE card_id=? AND kind='task_result'",
                           (child['id'],)).fetchone()
        return 'delivered' if row and row['published_at'] is not None and row['error'] is None else 'delivering'
    if store.is_final(child):
        return 'failed'
    if expired and not conn.execute('SELECT 1 FROM runs WHERE card_id=? LIMIT 1', (child['id'],)).fetchone():
        return 'starved'
    broken = conn.execute("""SELECT 1 FROM runs WHERE card_id=? AND role='worker' AND status IN ('timeout','crashed')
                             UNION ALL SELECT 1 FROM gates WHERE card_id=? AND passed=0""",
                          (child['id'], child['id'])).fetchone()
    return 'abandoned' if broken or expired else 'active'


def _hand_off(job, cfg, conn, card_id, handoff):
    """Create, stage and release the child of `handoff`; every step is idempotent so a retried tick resumes it."""
    child_id, _ = api.create(
        conn, title=job.title, goal=job.prompt, body=_body(job, handoff.gate), acceptance=_check_command(job, cfg),
        step=job.step, source=job.source, created_by=f'cron:{job.job}', idem_key=job.idem_key(handoff.gate),
        card_class='series_run', owner_visible=False, deliver='owner-inbox', not_before=time.time() + HOLD_S)
    child = api.card_row(conn, child_id)
    if child['state'] != 'created':
        return child_id
    series = _series(conn, card_id)
    payload = {'job_id': job.job, 'gate': handoff.gate, 'news_destination': job.policy.news_destination,
               'handoff': handoff.to_dict(), 'script_output': render(handoff)}
    workdir = heads.prepare_workspace(cfg, child)
    wg._atomic_write(workdir / 'input.json', json.dumps(payload, ensure_ascii=False, indent=2))
    with store.tx(conn):
        cur = conn.execute("INSERT OR IGNORE INTO links(card_id,kind,target_id,created_at) VALUES(?,'cron-agent',?,?)",
                           (child_id, series, time.time()))
        if cur.rowcount:
            store.event(conn, series, 'conditional_head_created',
                        {'gate': handoff.gate, 'card': child_id, 'job_id': job.job}, actor='watch')
    if child['not_before'] is not None:
        api.release(conn, child_id, actor='watch')
    return child_id


def _series(conn, card_id):
    if card_id is None:
        raise RuntimeError('a watch tick runs as a Hive series instance (HIVE_CARD is unset)')
    row = conn.execute("SELECT target_id FROM links WHERE card_id=? AND kind='series'", (card_id,)).fetchone()
    if row is None:
        raise RuntimeError(f'tick card {card_id} is not an instance of a series')
    return row['target_id']


def _check_command(job, cfg):
    return (f'PYTHONPATH={shlex.quote(str(REPO))} {shlex.quote(sys.executable)} -P -m hive.watch check '
            f'{shlex.quote(str(job.path))} --db {shlex.quote(str(cfg["db"]))}')


def _body(job, gate):
    return f'''Handoff {gate} of watch job {job.job}. The watch tick already ran the collector: read input.json in this
directory. `handoff.items` are the owed changes and `script_output` renders them. Do not rerun the collector.

Finish by writing three files here:
- result.json: {{"job_id": "{job.job}", "status": "ok", "message": "<human result, or exactly {SILENT}>",
  "news": <true only for substantive news for the owner, else false>, "evidence": ["<concrete check>", ...]}}
- REPORT.md: the message only.
- EVIDENCE.md: technical evidence, commands and limits.
A source failure or an unfinished check is not status ok and is never reported as no news.
Do not message the owner yourself; Hive delivers the result.
'''


def _policy(data, tz):
    _keys(data, POLICY_KEYS, {'news_destination', 'lease'}, 'policy')
    quiet = data.get('quiet')
    if quiet is not None:
        _keys(quiet, QUIET_KEYS, {'start', 'end'}, 'quiet')
        try:
            quiet = wg.QuietWindow(clock.fromisoformat(quiet['start']), clock.fromisoformat(quiet['end']),
                                   quiet.get('tz', tz), tuple(quiet.get('destinations', ['telegram'])))
        except (ValueError, TypeError, LookupError) as e:
            raise ConfigError(f'quiet window: {e}') from None
    try:
        return wg.Policy(data['news_destination'], data['lease'], quiet, data.get('wake_on_cleared', True),
                         data.get('error_grace', 0))
    except ValueError as e:
        raise ConfigError(str(e)) from None


def _keys(data, allowed, required, name):
    if not isinstance(data, dict):
        raise ConfigError(f'{name} must be an object')
    if set(data) - allowed or required - set(data):
        raise ConfigError(f'{name} takes {sorted(required)} and optionally {sorted(allowed - required)}')


def _positive(value, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ConfigError(f'{name} must be a positive number of seconds')
    return value


def _absolute(value, name):
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise ConfigError(f'{name} must be an absolute path')
    return Path(value)


def main(argv=None):
    ap = argparse.ArgumentParser(prog='python -P -m hive.watch')
    sub = ap.add_subparsers(dest='action', required=True)
    sub.add_parser('tick').add_argument('config')
    checking = sub.add_parser('check')
    checking.add_argument('config')
    checking.add_argument('--db', required=True)
    a = ap.parse_args(argv)
    card_id = os.environ.get('HIVE_CARD')
    cfg = store.load_config()
    if a.action == 'check':
        conn = store.connect(a.db, owner_actor=cfg['owner_actor'])
        try:
            print('delivery:', check(load(a.config, cfg['owner_tz']), conn, card_id, Path.cwd()))
        except (CheckError, wg.GateError) as e:
            print(f'watch check failed: {e}')
            return 1
        return 0
    conn = store.connect(cfg['db'], owner_actor=cfg['owner_actor'])
    try:
        print(json.dumps(tick(load(a.config, cfg['owner_tz']), cfg, conn, card_id, Path.cwd()), ensure_ascii=False))
    except Exception as e:
        # A failed tick reports to the secretary; the series stays local for silent ticks.
        if card_id is not None:
            with store.tx(conn):
                conn.execute("UPDATE cards SET deliver='owner-inbox' WHERE id=? AND state='running'", (card_id,))
        print(f'watch tick failed: {type(e).__name__}; see the run stderr log')
        raise
    return 0


if __name__ == '__main__':
    sys.exit(main())
