"""Shared plumbing of the watch collectors: bounded sources, stable signals and the `{"signals": [...]}` output.

A source is a callable taking its timeout in seconds and returning signal dicts under its covers, plus its own
`src:<name>` error for a partial reading. Sources run one after another inside a total budget; a failed, timed-out or
unparsable source becomes one `src:<name>` source_error whose fingerprint names only the failure class and whose
`covers` are that source's key prefixes.
"""
import contextlib
import hashlib
import json
import os
import re
import signal
import subprocess
import sys
import time
from datetime import datetime

from . import script, watch


def environment(cfg):
    """Inside a Hive card the tick already passed `script.environment`; a standalone run builds the same one."""
    if 'HIVE_CARD' in os.environ:
        return dict(os.environ)
    env = script.environment(cfg, None)
    del env['HIVE_CARD']
    return env


class SourceError(Exception):
    """A source that could not deliver a complete reading; the message is its stable failure class."""


def news(key, fingerprint, wake_at=None, bypass=False):
    item = {'key': key, 'class': 'news', 'fingerprint': fingerprint}
    if wake_at is not None:
        item['wake_at'] = wake_at
    if bypass:
        item['bypass'] = True
    return item


def text(value, limit=400):
    """Whitespace-collapsed text, cut to `limit` characters; None keeps all of it."""
    return ' '.join(str(value).split())[:limit]


def digitless(value, limit=400):
    """`text` with digit runs replaced, for messages that embed sizes, counts or times."""
    return re.sub(r'\d+', '#', text(value, limit))


def short_hash(value):
    return hashlib.sha256(value.encode()).hexdigest()[:16]


def epoch(value, name):
    """Epoch seconds of an ISO-8601 instant with an offset or Z."""
    if not isinstance(value, str) or not value:
        raise SourceError(f'{name} missing')
    try:
        moment = datetime.fromisoformat(value.replace('Z', '+00:00'))
    except ValueError:
        raise SourceError(f'{name} unparsable') from None
    if moment.tzinfo is None:
        raise SourceError(f'{name} has no offset')
    return moment.timestamp()


def merge(signals):
    """One signal per key: parts that share a key join their fingerprints in sorted order and keep the earliest
    wake_at; a key with any immediate part wakes immediately."""
    grouped = {}
    for item in signals:
        grouped.setdefault(item['key'], []).append(item)
    out = []
    for key in sorted(grouped):
        parts = grouped[key]
        if len(parts) == 1:
            out.append(parts[0])
            continue
        if any(p['class'] != 'news' for p in parts):
            raise ValueError(f'source_error key {key} repeats')
        waits = [p.get('wake_at') for p in parts]
        wake_at = None if None in waits else min(waits)
        out.append(news(key, '\n'.join(sorted({p['fingerprint'] for p in parts})), wake_at,
                        any(p.get('bypass', False) for p in parts)))
    return out


def command(argv, timeout, env):
    """Run argv in its own process group; returns (exit code, stdout, stderr). A timeout kills the group."""
    process = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, start_new_session=True)
    try:
        out, err = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        with contextlib.suppress(ProcessLookupError):
            os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise SourceError('timeout') from None
    return process.returncode, out.decode('utf-8', 'replace'), err.decode('utf-8', 'replace')


def json_output(raw):
    try:
        return json.loads(raw)
    except ValueError:
        raise SourceError('unparsable output') from None


@contextlib.contextmanager
def alarm(seconds):
    """Bound an in-process source; the main thread receives SourceError('timeout') when time runs out."""
    def fire(signum, frame):
        raise SourceError('timeout')
    previous = signal.signal(signal.SIGALRM, fire)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


class Source:
    def __init__(self, name, covers, timeout, read, in_process=False):
        self.name, self.covers, self.timeout = name, tuple(covers), timeout
        self.read, self.in_process = read, in_process

    @property
    def key(self):
        return f'src:{self.name}'

    def error(self, reason):
        return {'key': self.key, 'class': 'source_error', 'fingerprint': reason, 'covers': list(self.covers)}


def collect(sources, total, env, env_file, log=sys.stderr):
    """Run sources in order under `total` seconds; each gets min(its timeout, remaining). Returns merged signals,
    redacted of `env` secrets and the values of `env_file` (config `script_env_file`)."""
    deadline = time.monotonic() + total
    signals = []
    for source in sources:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            signals.append(source.error('skipped: total budget exhausted'))
            continue
        try:
            if source.in_process:
                with alarm(min(source.timeout, remaining)):
                    found = source.read(min(source.timeout, remaining))
            else:
                found = source.read(min(source.timeout, remaining))
        except SourceError as e:
            print(f'{source.key}: {e}', file=log)
            signals.append(source.error(str(e)))
            continue
        except (ValueError, TypeError, KeyError, AttributeError) as e:
            print(watch.redact(f'{source.key}: {type(e).__name__}: {e}', env, env_file), file=log)
            signals.append(source.error(f'invalid output: {type(e).__name__}'))
            continue
        outside = [s['key'] for s in found if s['key'] != source.key and not s['key'].startswith(source.covers)]
        if outside:
            raise ValueError(f'{source.key} produced keys outside its prefixes: {outside}')
        signals.extend(found)
    return merge([dict(s, fingerprint=watch.redact(s['fingerprint'], env, env_file)) for s in signals])


def dump(signals):
    """The collector's whole stdout: canonical JSON, keys sorted."""
    return json.dumps({'signals': sorted(signals, key=lambda s: s['key'])}, ensure_ascii=False, sort_keys=True,
                      separators=(',', ':')) + '\n'
