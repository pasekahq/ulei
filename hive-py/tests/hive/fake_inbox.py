"""In-memory owner inbox for the hive tests: the `packet`, `publish` and `get` calls of the inbox library.

Packets are byte-identical to the library's for the same producer key; `golden_c1_packets.json` pins them.
"""
import hashlib
import hmac
import json
import math
import re
import secrets
import time

FIELDS = frozenset({'version', 'kind', 'id', 'payload', 'source_ts', 'trust', 'mac'})
KINDS = frozenset({'weekly', 'task_result', 'task_blocked', 'provider_outage', 'provider_recovery'})
MAX_PAYLOAD = 65536
ID = re.compile(r'[a-zA-Z0-9:_-]{1,96}')


def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()


class FakeInbox:
    """Keeps every stored packet in `packets` by event id; `get` returns the stored row."""

    def __init__(self, key=None):
        self._key = secrets.token_bytes(32) if key is None else key
        if len(self._key) != 32:
            raise ValueError('invalid producer key')
        self.packets = {}
        self._rows = {}

    def _mac(self, body):
        return hmac.new(self._key, canonical(body), hashlib.sha256).hexdigest()

    def packet(self, kind, event_id, payload, source_ts):
        body = dict(version=1, kind=kind, id=event_id, payload=payload,
                    source_ts=source_ts, trust='untrusted-source-data')
        body['mac'] = self._mac(body)
        return body

    def publish(self, packet):
        if not isinstance(packet, dict) or set(packet) != FIELDS:
            raise ValueError('invalid packet fields')
        body = {k: v for k, v in packet.items() if k != 'mac'}
        if not isinstance(packet['mac'], str) or not hmac.compare_digest(packet['mac'], self._mac(body)):
            raise ValueError('unauthenticated packet')
        if (packet['version'] != 1 or packet['kind'] not in KINDS
                or packet['trust'] != 'untrusted-source-data'
                or not isinstance(packet['id'], str) or not ID.fullmatch(packet['id'])
                or not isinstance(packet['payload'], str) or not packet['payload'].strip()
                or len(packet['payload'].encode()) > MAX_PAYLOAD
                or type(packet['source_ts']) not in (int, float) or not math.isfinite(packet['source_ts'])
                or packet['source_ts'] < 0 or packet['source_ts'] > time.time() + 300):
            raise ValueError('invalid packet values')
        digest = hashlib.sha256(canonical(body)).hexdigest()
        row = self._rows.setdefault(packet['id'], {'id': packet['id'], 'packet': canonical(packet), 'digest': digest})
        if row['digest'] != digest:
            raise ValueError('event id already has different content')
        self.packets.setdefault(packet['id'], packet)
        return packet['id']

    def get(self, event_id):
        row = self._rows.get(event_id)
        return dict(row) if row else None
