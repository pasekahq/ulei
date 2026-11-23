"""Script-first watch gate: decide from normalized signals whether a model head must start.

`evaluate` is pure. The acknowledged baseline advances only through `ack` of a handed-off gate id;
items of a failed, abandoned or expired handoff, and items recorded by `observe`, are owed and
re-delivered until acknowledged, even after their signal vanished.

Two opt-in policy rules keep a model head for real news only. `wake_on_cleared=False`: a cleared key rides along with
a wake but never causes one; when nothing else changed it is absorbed into the baseline at once. `error_grace`: a
`source_error` that newly appears or changes wakes only once it has been present for that many seconds, so a source
that flaps from tick to tick stays silent while a persistent failure still reaches the owner-inbox.
"""
import contextlib
import fcntl
import hashlib
import json
import math
import os
import tempfile
from dataclasses import dataclass, field, replace
from datetime import datetime, time as clock
from pathlib import Path
from zoneinfo import ZoneInfo

FORMAT_VERSION = 1
CLASSES = ('news', 'source_error')
CHANGES = ('new', 'changed', 'cleared', 'threshold')
DESTINATIONS = ('telegram', 'owner-inbox', 'local')
ERROR_DESTINATION = 'owner-inbox'


class GateError(Exception):
    """A gate call that contradicts the checkpoint state."""


class CheckpointMissing(GateError):
    pass


class CheckpointCorrupt(GateError):
    pass


@dataclass(frozen=True)
class Signal:
    """One collector item. `fingerprint` must already exclude timestamps, durations, counters and PIDs.
    While a `source_error` signal is present, absent baseline keys under its `covers` prefixes are not cleared.
    `bypass` marks a direct question or consent request that a quiet window never holds."""
    key: str
    cls: str
    fingerprint: str
    wake_at: float | None = None
    covers: tuple[str, ...] = ()
    bypass: bool = False

    def __post_init__(self):
        if not isinstance(self.key, str) or not self.key:
            raise ValueError('signal key must be a non-empty string')
        if self.cls not in CLASSES:
            raise ValueError(f'signal {self.key}: class must be one of {CLASSES}')
        if not isinstance(self.fingerprint, str):
            raise ValueError(f'signal {self.key}: fingerprint must be a string')
        _check_instant(self.wake_at, f'signal {self.key}: wake_at')
        object.__setattr__(self, 'covers', tuple(self.covers))
        if self.covers and self.cls != 'source_error':
            raise ValueError(f'signal {self.key}: only a source_error signal covers keys')
        if any(not isinstance(p, str) or not p for p in self.covers):
            raise ValueError(f'signal {self.key}: covers must be non-empty key prefixes')
        if not isinstance(self.bypass, bool):
            raise ValueError(f'signal {self.key}: bypass must be a bool')

    @property
    def digest(self):
        return _digest(self.key, self.fingerprint)

    def to_dict(self):
        return {'key': self.key, 'class': self.cls, 'fingerprint': self.fingerprint, 'digest': self.digest,
                'wake_at': self.wake_at, 'covers': list(self.covers), 'bypass': self.bypass}

    @classmethod
    def from_dict(cls, data):
        if not isinstance(data['covers'], list):
            raise ValueError('bad signal covers')
        return cls(data['key'], data['class'], data['fingerprint'], data['wake_at'], tuple(data['covers']),
                   data['bypass'])


@dataclass(frozen=True)
class Record:
    """Acknowledged state of one key; `fired` is the wake_at whose crossing was delivered."""
    cls: str
    fingerprint: str
    wake_at: float | None = None
    fired: float | None = None

    def to_dict(self):
        return {'class': self.cls, 'fingerprint': self.fingerprint, 'wake_at': self.wake_at, 'fired': self.fired}

    @classmethod
    def from_dict(cls, data):
        if data['class'] not in CLASSES or not isinstance(data['fingerprint'], str):
            raise ValueError('bad record')
        _check_instant(data['wake_at'], 'wake_at')
        _check_instant(data['fired'], 'fired')
        return cls(data['class'], data['fingerprint'], data['wake_at'], data['fired'])


@dataclass(frozen=True)
class Item:
    """One semantic change. `after` is the record an ack stores; None clears the key."""
    key: str
    change: str
    cls: str
    destination: str
    before: Record | None
    after: Record | None
    held: bool = False
    redelivered: bool = False
    bypass: bool = False

    @property
    def identity(self):
        """Owed items with the same key and resulting record carry the same news."""
        return self.key, self.after

    def to_dict(self):
        digest = _digest(self.key, self.after.fingerprint) if self.after else None
        return {'key': self.key, 'change': self.change, 'class': self.cls, 'destination': self.destination,
                'held': self.held, 'redelivered': self.redelivered, 'bypass': self.bypass, 'digest': digest,
                'before': self.before.to_dict() if self.before else None,
                'after': self.after.to_dict() if self.after else None}

    @classmethod
    def from_dict(cls, data):
        flags = (data['held'], data['redelivered'], data['bypass'])
        if (not isinstance(data['key'], str) or not data['key'] or data['change'] not in CHANGES
                or data['class'] not in CLASSES or data['destination'] not in DESTINATIONS
                or not all(isinstance(f, bool) for f in flags)):
            raise ValueError('bad item')
        before = Record.from_dict(data['before']) if data['before'] is not None else None
        after = Record.from_dict(data['after']) if data['after'] is not None else None
        return cls(data['key'], data['change'], data['class'], data['destination'], before, after, *flags)


@dataclass(frozen=True)
class QuietWindow:
    """Local hours [start, end) in `tz` during which news for `destinations` is held, never source errors.
    Each job passes its own window."""
    start: clock
    end: clock
    tz: str
    destinations: tuple[str, ...] = ('telegram',)

    def __post_init__(self):
        ZoneInfo(self.tz)
        if not isinstance(self.start, clock) or not isinstance(self.end, clock):
            raise ValueError('quiet window start and end must be datetime.time values')
        if self.start == self.end:
            raise ValueError('quiet window start and end must differ')
        if any(d not in DESTINATIONS for d in self.destinations):
            raise ValueError(f'quiet window destinations must be among {DESTINATIONS}')

    def contains(self, now):
        local = datetime.fromtimestamp(now, ZoneInfo(self.tz)).time()
        if self.start < self.end:
            return self.start <= local < self.end
        return local >= self.start or local < self.end


@dataclass(frozen=True)
class Policy:
    """`lease` bounds how long an unacknowledged handoff counts as in flight before it is owed again."""
    news_destination: str
    lease: float
    quiet: QuietWindow | None = None
    wake_on_cleared: bool = True
    error_grace: float = 0

    def __post_init__(self):
        if self.news_destination not in DESTINATIONS:
            raise ValueError(f'news destination must be one of {DESTINATIONS}')
        if not isinstance(self.wake_on_cleared, bool):
            raise ValueError('wake_on_cleared must be a bool')
        grace = self.error_grace
        if isinstance(grace, bool) or not isinstance(grace, (int, float)) or not math.isfinite(grace) or grace < 0:
            raise ValueError('error_grace must be a non-negative number of seconds')
        lease = self.lease
        if isinstance(lease, bool) or not isinstance(lease, (int, float)) or not math.isfinite(lease) or lease <= 0:
            raise ValueError('lease must be a positive number of seconds')

    def destination(self, cls):
        return ERROR_DESTINATION if cls == 'source_error' else self.news_destination

    def holds(self, cls, destination, now):
        return (cls == 'news' and self.quiet is not None and destination in self.quiet.destinations
                and self.quiet.contains(now))

    def wakes(self, item):
        """Whether `item` alone is reason enough to start a head."""
        return not item.held and (self.wake_on_cleared or item.change != 'cleared')


@dataclass(frozen=True)
class Handoff:
    gate: str
    lease_until: float
    items: tuple[Item, ...]
    signals: tuple[Signal, ...]

    def to_dict(self):
        return {'gate': self.gate, 'lease_until': self.lease_until, 'items': [i.to_dict() for i in self.items],
                'signals': [s.to_dict() for s in self.signals]}

    @classmethod
    def from_dict(cls, data):
        if not isinstance(data['gate'], str) or not data['gate']:
            raise ValueError('bad gate id')
        if data['lease_until'] is None or not isinstance(data['items'], list) or not isinstance(data['signals'], list):
            raise ValueError('bad handoff')
        _check_instant(data['lease_until'], 'lease_until')
        return cls(data['gate'], data['lease_until'], tuple(Item.from_dict(i) for i in data['items']),
                   tuple(Signal.from_dict(s) for s in data['signals']))


@dataclass(frozen=True)
class Checkpoint:
    baseline: dict = field(default_factory=dict)
    owed: tuple[Item, ...] = ()
    pending: Handoff | None = None
    suspects: dict = field(default_factory=dict)  # source_error key -> epoch it was first seen, see Policy.error_grace

    def to_dict(self):
        data = {'version': FORMAT_VERSION,
                'baseline': {k: self.baseline[k].to_dict() for k in sorted(self.baseline)},
                'owed': [i.to_dict() for i in self.owed],
                'pending': self.pending.to_dict() if self.pending else None}
        if self.suspects:
            data['suspects'] = {k: self.suspects[k] for k in sorted(self.suspects)}
        return data

    def to_json(self):
        return _canonical(self.to_dict())

    @classmethod
    def from_dict(cls, data):
        version = data.get('version') if isinstance(data, dict) else None
        if isinstance(version, bool) or version != FORMAT_VERSION:
            raise ValueError('unsupported checkpoint version')
        if not isinstance(data['baseline'], dict) or not isinstance(data['owed'], list):
            raise ValueError('bad checkpoint layout')
        pending = Handoff.from_dict(data['pending']) if data['pending'] is not None else None
        suspects = data.get('suspects', {})
        if not isinstance(suspects, dict):
            raise ValueError('bad checkpoint suspects')
        for key, first in suspects.items():
            _check_instant(first, f'suspect {key}')
            if first is None:
                raise ValueError('bad checkpoint suspects')
        return cls({k: Record.from_dict(v) for k, v in data['baseline'].items()},
                   tuple(Item.from_dict(i) for i in data['owed']), pending, dict(suspects))


@dataclass(frozen=True)
class Decision:
    """`gate` names the in-flight handoff for `wait-active`; held items never start a head. `suspects` is the
    checkpoint's next error-grace map; `absorbed` are cleared items of a silent decision that `observe` folds into the
    baseline instead of owing them."""
    action: str
    items: tuple[Item, ...]
    signals: tuple[Signal, ...]
    gate: str | None = None
    suspects: dict = field(default_factory=dict)
    absorbed: tuple[Item, ...] = ()

    @property
    def handed(self):
        return tuple(i for i in self.items if not i.held)

    def to_dict(self):
        return {'action': self.action, 'gate': self.gate, 'items': [i.to_dict() for i in self.items]}

    def to_json(self):
        return _canonical(self.to_dict())


def baseline(signals, now, seen=None):
    """A checkpoint that treats the given signals as already delivered, optionally only keys in legacy `seen`."""
    current = _index(signals)
    if seen is not None:
        seen = set(seen)
        current = {k: s for k, s in current.items() if k in seen}
    return Checkpoint({k: _record(s, now) for k, s in current.items()})


def evaluate(checkpoint, signals, now, policy):
    """Return the decision for `signals` at `now`; performs no I/O."""
    _check_instant(now, 'now')
    current = _index(signals)
    ordered = tuple(current[k] for k in sorted(current))
    in_flight = checkpoint.pending if checkpoint.pending and now < checkpoint.pending.lease_until else None
    fresh, suspects = _settle_errors(_diff(checkpoint.baseline, current, now, policy), checkpoint.suspects, now, policy)
    news = {i.identity for i in fresh}
    owed_now = _owed(checkpoint, now)
    # An owed "cleared" whose key is present again is stale news for a policy that does not wake on cleared.
    stale = () if policy.wake_on_cleared else tuple(i for i in owed_now if i.change == 'cleared' and i.key in current)
    owed = [_redeliver(i, now, policy) for i in owed_now if i.identity not in news and i not in stale]
    items = tuple(sorted(owed + fresh, key=lambda i: (i.key, not i.redelivered)))
    if in_flight is not None:
        return Decision('wait-active', items, ordered, in_flight.gate, suspects, stale)
    if any(policy.wakes(i) for i in items):
        return Decision('wake', items, ordered, suspects=suspects, absorbed=stale)
    gone = tuple(i for i in items if i.change == 'cleared' and not i.held and not policy.wake_on_cleared)
    return Decision('silent', items, ordered, suspects=suspects, absorbed=stale + gone)


def _settle_errors(fresh, suspects, now, policy):
    """Drop the new or changed `source_error` items younger than `error_grace`; returns (items, next suspects)."""
    if not policy.error_grace:
        return fresh, {}
    kept, seen = [], {}
    for item in fresh:
        if item.cls == 'source_error' and item.change in ('new', 'changed'):
            first = seen[item.key] = suspects.get(item.key, now)
            if now - first < policy.error_grace:
                continue
        kept.append(item)
    return kept, seen


def begin(checkpoint, decision, gate, now, policy):
    """Record `decision`'s handed items as the pending handoff `gate` and owe its held items; the baseline does not
    move."""
    if decision.action != 'wake':
        raise GateError(f'cannot hand off a {decision.action} decision')
    if not isinstance(gate, str) or not gate:
        raise GateError('gate id must be a non-empty string')
    if checkpoint.pending is not None and now < checkpoint.pending.lease_until:
        raise GateError(f'handoff {checkpoint.pending.gate} is still in flight')
    handoff = Handoff(gate, now + policy.lease, decision.handed, decision.signals)
    gone = {i.identity for i in decision.absorbed}
    owed = tuple(i for i in _owed(checkpoint, now) if i.identity not in gone)
    return Checkpoint(checkpoint.baseline, _merge(owed, (i for i in decision.items if i.held)), handoff,
                      decision.suspects)


def observe(checkpoint, decision):
    """Owe every item of a decision that `begin` does not hand off (`wait-active`, held), so it is delivered later
    even if its signal vanishes first. Absorbed cleared items leave the owed list and, unless their key is present
    again, the baseline; the error-grace map moves to the decision's."""
    base, owed = checkpoint.baseline, checkpoint.owed
    if decision.absorbed:
        present = {s.key for s in decision.signals}
        base = {k: v for k, v in base.items() if k in present or k not in {i.key for i in decision.absorbed}}
        gone = {i.identity for i in decision.absorbed}
        owed = tuple(i for i in owed if i.identity not in gone)
    pending = tuple(i for i in decision.items if i not in decision.absorbed)
    return Checkpoint(base, _merge(owed, pending), checkpoint.pending, decision.suspects)


def ack(checkpoint, gate):
    """Advance the baseline by handoff `gate` after its result was accepted and delivered."""
    handoff = _pending(checkpoint, gate)
    base = dict(checkpoint.baseline)
    for item in handoff.items:
        if item.redelivered:
            continue
        if item.after is None:
            base.pop(item.key, None)
        else:
            base[item.key] = item.after
    handed = {i.identity for i in handoff.items}
    suspects = {k: v for k, v in checkpoint.suspects.items() if k not in {i.key for i in handoff.items}}
    return Checkpoint(base, tuple(i for i in checkpoint.owed if i.identity not in handed), None, suspects)


def fail(checkpoint, gate):
    """Mark handoff `gate` failed or abandoned; its items are owed to the next wake."""
    handoff = _pending(checkpoint, gate)
    return Checkpoint(checkpoint.baseline, _merge(checkpoint.owed, handoff.items), None, checkpoint.suspects)


def read(path):
    """Load a checkpoint without locking or writing; missing and corrupt files raise."""
    path = Path(path)
    try:
        text = path.read_text(encoding='utf-8')
    except FileNotFoundError:
        raise CheckpointMissing(f'{path}: checkpoint missing; pass an explicit baseline to start') from None
    try:
        return Checkpoint.from_dict(json.loads(text))
    except (ValueError, KeyError, TypeError) as e:
        raise CheckpointCorrupt(f'{path}: corrupt checkpoint: {e}') from None


class Transaction:
    def __init__(self, path, checkpoint):
        self.path = path
        self.checkpoint = checkpoint
        self.open = True

    def commit(self, checkpoint):
        if not self.open:
            raise GateError('transaction is closed')
        _atomic_write(self.path, checkpoint.to_json())
        self.checkpoint = checkpoint


@contextlib.contextmanager
def locked(path, baseline=None):
    """Hold the exclusive lock of `path` and yield a Transaction. `baseline` is written only when the file is missing."""
    path = Path(path)
    with open(path.with_name(path.name + '.lock'), 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            checkpoint = read(path)
        except CheckpointMissing:
            if baseline is None:
                raise
            _atomic_write(path, baseline.to_json())
            checkpoint = baseline
        tx = Transaction(path, checkpoint)
        try:
            yield tx
        finally:
            tx.open = False


def _diff(base, current, now, policy):
    items = []
    covered = tuple(p for s in current.values() if s.cls == 'source_error' for p in s.covers)
    for key in sorted(set(base) | set(current)):
        before, signal = base.get(key), current.get(key)
        if signal is None:
            if not key.startswith(covered):
                items.append(_item(key, 'cleared', before.cls, before, None, now, policy))
            continue
        if signal.wake_at is not None and now < signal.wake_at:
            continue
        after = _record(signal, now)
        if before is None:
            change = 'new' if signal.wake_at is None else 'threshold'
        elif before.cls != signal.cls or before.fingerprint != signal.fingerprint:
            change = 'changed'
        elif signal.wake_at is not None and before.fired != signal.wake_at:
            change = 'threshold'
        else:
            continue
        items.append(_item(key, change, signal.cls, before, after, now, policy, signal.bypass))
    return items


def _item(key, change, cls, before, after, now, policy, bypass=False, redelivered=False):
    destination = policy.destination(cls)
    held = not bypass and policy.holds(cls, destination, now)
    return Item(key, change, cls, destination, before, after, held, redelivered, bypass)


def _redeliver(item, now, policy):
    return _item(item.key, item.change, item.cls, item.before, item.after, now, policy, item.bypass, redelivered=True)


def _record(signal, now):
    fired = signal.wake_at if signal.wake_at is not None and now >= signal.wake_at else None
    return Record(signal.cls, signal.fingerprint, signal.wake_at, fired)


def _owed(checkpoint, now):
    if checkpoint.pending is not None and now >= checkpoint.pending.lease_until:
        return _merge(checkpoint.owed, checkpoint.pending.items)
    return checkpoint.owed


def _merge(older, newer):
    """Owed items in arrival order, one per identity; flags are recomputed at delivery."""
    merged = {i.identity: i for i in older}
    merged.update((i.identity, replace(i, held=False, redelivered=False)) for i in newer)
    return tuple(merged.values())


def _pending(checkpoint, gate):
    if checkpoint.pending is None or checkpoint.pending.gate != gate:
        raise GateError(f'no pending handoff {gate}')
    return checkpoint.pending


def _index(signals):
    current = {}
    for signal in signals:
        if not isinstance(signal, Signal):
            raise TypeError('signals must be Signal instances')
        if signal.key in current:
            raise ValueError(f'duplicate signal key {signal.key}')
        current[signal.key] = signal
    return current


def _digest(key, fingerprint):
    return hashlib.sha256(key.encode() + b'\0' + fingerprint.encode()).hexdigest()


def _check_instant(value, name):
    if value is None:
        return
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError(f'{name} must be a finite epoch number')


def _canonical(data):
    return json.dumps(data, ensure_ascii=False, sort_keys=True, separators=(',', ':'))


def _atomic_write(path, text):
    fd, temp = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.', suffix='.tmp')
    try:
        with os.fdopen(fd, 'w', encoding='utf-8') as stream:
            stream.write(text)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, path)
    except BaseException:
        with contextlib.suppress(FileNotFoundError):
            os.unlink(temp)
        raise
    directory = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(directory)
    finally:
        os.close(directory)
