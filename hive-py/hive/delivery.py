"""The one way hive events reach the secretary: `open_adapter(cfg)` gives the adapter config `delivery` names."""
import hashlib
import hmac
import json
import os
import sys
from pathlib import Path
from typing import NamedTuple

from .c4g import UnixConnection

KEY_LEN = 32
# Per socket operation, as the other C1 producers of the repository.
TIMEOUT_S = 5


class Event(NamedTuple):
    """One event for the secretary: a C1 kind, its id, its JSON text payload and the source time."""
    kind: str
    id: str
    payload: str
    source_ts: float


def require(cfg, keys):
    from .store import missing  # store imports this module to check the config
    for key in keys:
        if key not in cfg:
            raise missing(key)


class Drop:
    """Publishes into the owner inbox through the deployment's library: the module `inbox` in the directory
    config `inbox_library` names, its `Inbox` on the directory config `inbox` names."""
    KEYS = ('inbox_library', 'inbox')

    def __init__(self, cfg):
        library = str(cfg['inbox_library'])
        if library not in sys.path:
            sys.path.insert(0, library)
        from inbox import Inbox
        self.inbox = Inbox(Path(cfg['inbox']))

    @classmethod
    def check(cls, cfg):
        require(cfg, cls.KEYS)

    def deliver(self, event):
        """Publish `event` and read it back; raises when the inbox refuses it or does not return it."""
        self.inbox.publish(self.inbox.packet(event.kind, event.id, event.payload, event.source_ts))
        if self.inbox.get(event.id) is None:
            raise RuntimeError('inbox readback failed')

    def delivered(self, event_id):
        return self.inbox.get(event_id) is not None


class C1Error(Exception):
    """The owner inbox refused a C1 call or answered outside the contract."""


def canonical(value):
    """C1 canonical JSON: sorted keys, no spaces, UTF-8."""
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()


def producer_key(cfg):
    """The key in the file config `inbox_producer_key_file` names: exactly 32 bytes, open to no group or other user."""
    path = cfg['inbox_producer_key_file']
    where = f'config: inbox_producer_key_file {path}'
    try:
        with open(path, 'rb') as f:
            mode = os.fstat(f.fileno()).st_mode & 0o777
            if mode & 0o077:
                raise ValueError(f'{where}: open to group or others (mode {mode:o})')
            key = f.read(KEY_LEN + 1)
    except OSError as e:
        raise ValueError(f'{where}: {e.strerror}') from e
    if len(key) != KEY_LEN:
        raise ValueError(f'{where}: must hold {KEY_LEN} bytes, got {len(key)}')
    return key


class Ulei:
    """C1 client of the owner inbox on the socket config `inbox_socket` names, signing with `producer_key(cfg)`.
    Its packets are byte for byte those the drop adapter's library makes of the same event and key."""
    KEYS = ('inbox_socket', 'inbox_producer_key_file')

    def __init__(self, cfg):
        self.socket = cfg['inbox_socket']
        self.key = producer_key(cfg)

    @classmethod
    def check(cls, cfg):
        require(cfg, cls.KEYS)
        for key in cls.KEYS:
            if not isinstance(cfg[key], str) or not cfg[key]:
                raise ValueError(f'config: {key} must be a path, got {cfg[key]!r}')
        producer_key(cfg)

    def packet(self, event):
        body = dict(version=1, kind=event.kind, id=event.id, payload=event.payload,
                    source_ts=event.source_ts, trust='untrusted-source-data')
        body['mac'] = hmac.new(self.key, canonical(body), hashlib.sha256).hexdigest()
        return body

    def call(self, method, body):
        """`POST /v1/<method>` with `body` on a new connection: the JSON object of a 2xx answer; raises C1Error with
        the code and message of a C1 `Error`, or on an answer outside the contract."""
        conn = UnixConnection(self.socket, TIMEOUT_S)
        try:
            conn.request('POST', f'/v1/{method}', canonical(body), {'Content-Type': 'application/json'})
            resp = conn.getresponse()
            status, raw = resp.status, resp.read()
        finally:
            conn.close()
        try:
            answer = json.loads(raw)
        except ValueError:
            answer = None
        if not isinstance(answer, dict):
            raise C1Error(f'{method}: answer {status} is not a JSON object')
        if 200 <= status < 300:
            return answer
        if set(answer) != {'code', 'message'} or not all(isinstance(v, str) for v in answer.values()):
            raise C1Error(f'{method}: answer {status} is not a C1 Error')
        raise C1Error(f"{method}: {status} {answer['code']}: {answer['message']}")

    def deliver(self, event):
        """One C1 `publish` of the event's packet; raises unless the inbox answers that it holds the event."""
        if self.call('publish', {'packet': self.packet(event)}) != {'id': event.id}:
            raise C1Error(f'publish: answer is not the PublishResponse of {event.id}')

    def delivered(self, event_id):
        """C1 `get`: true exactly when the inbox returns a row."""
        answer = self.call('get', {'id': event_id})
        if not set(answer) <= {'row'} or not isinstance(answer.get('row', {}), dict):
            raise C1Error('get: answer is not a GetResponse')
        return 'row' in answer


ADAPTERS = {'drop': Drop, 'ulei': Ulei}


def check(cfg):
    """Refuse a config whose `delivery` names no adapter, or that lacks or holds a bad value of a key that adapter
    reads."""
    name = cfg['delivery']
    if not isinstance(name, str) or name not in ADAPTERS:
        raise ValueError(f'config: delivery must be one of {", ".join(ADAPTERS)}, got {name!r}')
    ADAPTERS[name].check(cfg)


def open_adapter(cfg):
    """The adapter config `delivery` names: `deliver(event)` and `delivered(event_id)`."""
    return ADAPTERS[cfg['delivery']](cfg)
