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
