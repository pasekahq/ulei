# C1 golden vectors

`vectors.json` pins the canonical JSON, the stored digest and the HMAC of C1 packets byte for
byte to the Python inbox. It is produced by the snippet below, which uses the expressions of
the Python inbox and the public test key of `../../README.md`.

Fields of each vector:

| Field | Meaning |
|---|---|
| `name` | Case name. |
| `rust` | `reproduce`: a Rust inbox accepts the packet and produces exactly these bytes. `refuse`: a Rust inbox rejects `publish` of the packet with `invalid_packet`. |
| `rule` | The canonical-JSON rule of ADR-0007 that decides `rust`. |
| `packet` | The packet without `mac`. |
| `canonical_hex` | `canonical(packet)`, hex: the bytes under the digest and the HMAC. |
| `digest` | `sha256_hex(canonical(packet))`, the `digest` column. |
| `mac` | `hmac_sha256_hex(key, canonical(packet))`. |
| `stored_hex` | `canonical(packet with mac)`, hex: the `packet` column. |

Regenerate with `python3 -I gen.py > vectors.json`, where `gen.py` holds:

```python
import hashlib
import hmac
import json
import sys


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()


KEY = bytes(range(32))
TS = 1763290000
LONGEST_ID = ('golden:Aa0_-' * 9)[:96]
CASES = [
    # name, rust, ADR-0007 canonical rule, kind, id, payload, source_ts
    ('int_source_ts', 'reproduce', 2, 'weekly', 'golden:int', 'Weekly digest.', TS),
    ('float_point_zero', 'reproduce', 2, 'task_result', 'golden:float-zero', 'Card finished.', 1763290000.0),
    ('float_fraction', 'reproduce', 3, 'task_result', 'golden:float-half', 'Card finished.', 1763290000.5),
    ('float_full_precision', 'reproduce', 3, 'task_result', 'golden:float-full', 'Card finished.', 1763000362.6592963),
    ('float_fixed_lower_bound', 'reproduce', 4, 'task_blocked', 'golden:float-1e-4', 'Card blocked.', 0.0001),
    ('float_exponent', 'refuse', 4, 'task_blocked', 'golden:float-1e-5', 'Card blocked.', 1e-05),
    ('non_ascii', 'reproduce', 1, 'weekly', 'golden:non-ascii', 'Привет, неделя прошла 🦀 спокойно', TS),
    ('quote_backslash', 'reproduce', 1, 'task_result', 'golden:quote', 'He said "done" in C:\\work\\x', TS),
    ('newline_control', 'reproduce', 1, 'provider_outage', 'golden:control', 'line one\nline two\ttab\x01soh\x1fus\x7fdel', TS),
    ('line_separator', 'reproduce', 1, 'provider_recovery', 'golden:u2028', 'before\u2028after\u2029end', TS),
    ('slash', 'reproduce', 1, 'weekly', 'golden:slash', 'path a/b and </script>', TS),
    ('longest_id', 'reproduce', 1, 'weekly', LONGEST_ID, 'Longest valid id.', TS),
]

vectors = []
for name, rust, rule, kind, event_id, payload, source_ts in CASES:
    packet = dict(version=1, kind=kind, id=event_id, payload=payload,
                  source_ts=source_ts, trust='untrusted-source-data')
    body = canonical(packet)
    mac = hmac.new(KEY, body, hashlib.sha256).hexdigest()
    vectors.append({
        'name': name,
        'rust': rust,
        'rule': f'ADR-0007 canonical rule {rule}',
        'packet': packet,
        'canonical_hex': body.hex(),
        'digest': hashlib.sha256(body).hexdigest(),
        'mac': mac,
        'stored_hex': canonical(dict(packet, mac=mac)).hex(),
    })
json.dump({'key_hex': KEY.hex(), 'vectors': vectors}, sys.stdout, indent=2)
print()
```

## Golden database

`events.before.sqlite3` is an `events.sqlite3` written by the Python `Inbox` class under the
test key, with `time.time()` fixed by the generator and `secrets.token_hex` replaced by a
counter hash, so every byte is reproducible. It holds one row in each state: `pending` (two,
from publish alone), `pending` with a future `available` after a retry, `leased`,
`processing`, `done` (`success`, `silent`), `uncertain` (`uncertain`, `expired-processing`,
`deferred-exhausted`) and `deferred`. `events.after.sqlite3` is a copy of it after the
operation script below (from `# After:`): an idempotent re-publish, `renew`, `start`, `ack` of
each outcome, `claim`, `retry` and `recover`.

`rows.before.json` and `rows.after.json` hold every row of the matching database, ordered by
`id`, each column as `{"type": typeof(column), "value": value}` with a blob in lowercase hex.
Lease tokens are random in a live inbox, so a replay compares only whether a lease is present
and that it is 32 lowercase hex characters.

Regenerate with `python3 -I gen_db.py <directory of the Python inbox.py> <this directory>`,
where `gen_db.py` holds:

```python
import hashlib
import json
import os
from pathlib import Path
import secrets
import shutil
import sqlite3
import sys
import tempfile
import time

sys.path.insert(0, sys.argv[1])
from inbox import Inbox

OUT = Path(sys.argv[2])
KEY = bytes(range(32))
CLOCK = [0.0]
time.time = lambda: CLOCK[0]
TOKENS = iter(range(1, 1000))
secrets.token_hex = lambda n: hashlib.sha256(b'golden-lease-%d' % next(TOKENS)).hexdigest()[:2 * n]

T = 1763290000.0
U = 1763301000.123456
COLUMNS = ('id', 'packet', 'digest', 'status', 'attempts', 'lease', 'lease_until',
           'available', 'session_id', 'outcome', 'created')
PACKETS = [
    # kind, id, payload, source_ts
    ('task_result', 'golden:exhausted', 'Card finished after three offers.', 1763289990),
    ('task_blocked', 'golden:expired', 'Card blocked on review.', 1763289995.5),
    ('weekly', 'golden:done-success', 'Weekly digest.', 1763300990),
    ('provider_outage', 'golden:done-silent', 'Provider down.', 1763300991),
    ('provider_recovery', 'golden:uncertain', 'Provider back.', 1763300992.0),
    ('task_result', 'golden:deferred', 'Card result deferred.', 1763300993.6592963),
    ('task_blocked', 'golden:retried', 'Card blocked, retry later.', 1763300994),
    ('weekly', 'golden:processing', 'Weekly digest in progress.', 1763300995.25),
    ('task_result', 'golden:leased', 'Card finished, leased.', 1763300996),
    ('weekly', 'golden:pending-a', 'Привет, неделя прошла 🦀 спокойно', 1763300997.125),
    ('provider_outage', 'golden:pending-b', 'line one\nline "two"\tend', 1763300998),
]


def at(now):
    CLOCK[0] = now


def key_root(root):
    root.mkdir(mode=0o700)
    fd = os.open(root / 'producer.key', os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'wb') as f:
        f.write(KEY)
    return root


def dump(path):
    db = sqlite3.connect(path)
    select = ','.join(f'{c},typeof({c})' for c in COLUMNS)
    rows = []
    for r in db.execute(f'SELECT {select} FROM events ORDER BY id'):
        rows.append({c: {'type': r[2 * i + 1], 'value': r[2 * i].hex() if r[2 * i + 1] == 'blob' else r[2 * i]}
                     for i, c in enumerate(COLUMNS)})
    db.close()
    return {'rows': rows}


def save(name, path):
    shutil.copyfile(path, OUT / f'events.{name}.sqlite3')
    with open(OUT / f'rows.{name}.json', 'w', encoding='utf-8') as f:
        json.dump(dump(path), f, ensure_ascii=False, indent=1)
        f.write('\n')


work = Path(tempfile.mkdtemp())
inbox = Inbox(key_root(work / 'before'))
packets = {i: inbox.packet(k, i, p, ts) for k, i, p, ts in PACKETS}


def publish(event_id, now):
    at(now)
    assert inbox.publish(packets[event_id]) == event_id


def claim(event_id, now):
    row = inbox.claim(now)
    assert row['id'] == event_id
    return row


def settle(row, outcome, now):
    assert inbox.start(row, f'session:{row["id"]}')
    at(now)
    assert inbox.ack(row, outcome)


# Before: every status, built along the Python lease edges.
publish('golden:exhausted', T)
settle(claim('golden:exhausted', T), 'deferred', T + 1)
settle(claim('golden:exhausted', T + 3601.5), 'deferred', T + 3602)
settle(claim('golden:exhausted', T + 7202.5), 'deferred', T + 7203)
inbox.recover(T + 10803.5)
publish('golden:expired', T + 10804)
assert inbox.start(claim('golden:expired', T + 10804.5), 'session:golden:expired')
inbox.recover(T + 10925)
publish('golden:done-success', U)
settle(claim('golden:done-success', U + 1), 'success', U + 2)
publish('golden:done-silent', U + 3)
settle(claim('golden:done-silent', U + 4), 'silent', U + 5)
publish('golden:uncertain', U + 6)
settle(claim('golden:uncertain', U + 7), 'uncertain', U + 8)
publish('golden:deferred', U + 9)
settle(claim('golden:deferred', U + 10), 'deferred', U + 11)
publish('golden:retried', U + 12)
assert inbox.retry(claim('golden:retried', U + 13), U + 13)
publish('golden:processing', U + 14)
assert inbox.start(claim('golden:processing', U + 15), 'session:golden:processing')
publish('golden:leased', U + 16)
claim('golden:leased', U + 17)
publish('golden:pending-a', U + 18)
publish('golden:pending-b', U + 19)
save('before', inbox.path)

# After: the operation script on a copy of the before file.
shutil.copyfile(inbox.path, key_root(work / 'after') / 'events.sqlite3')
inbox = Inbox(work / 'after')
publish('golden:pending-a', U + 40)
leased, processing = inbox.get('golden:leased'), inbox.get('golden:processing')
assert inbox.renew(leased, U + 40, ttl=60)
assert inbox.renew(processing, U + 40)
settle(leased, 'success', U + 41)
at(U + 42)
assert inbox.ack(processing, 'uncertain')
settle(claim('golden:retried', U + 43), 'silent', U + 44)
assert inbox.retry(claim('golden:pending-a', U + 45), U + 45)
settle(claim('golden:pending-b', U + 46), 'deferred', U + 47)
inbox.recover(U + 3612)
save('after', inbox.path)
shutil.rmtree(work)
```
