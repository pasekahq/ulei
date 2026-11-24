"""Stdlib client of the qb model gateway (C4G): one `POST /v1/model/turn` over HTTP/1.1 on a unix socket.

Every outcome is a value: `Text` for a turn that ended with `stop`, `Failure` for an `error` event, a flat
pre-turn `Error` body, or a transport or protocol failure.
"""
import http.client
import json
import socket
import threading
from dataclasses import dataclass

TURN_PATH = '/v1/model/turn'
# `no_login_for_tag`: qb's draft refusal of a tagged request (ADR-0068 section 2.6, item 4), not in the vendored C4G.
ERROR_CLASSES = ('quota_exhausted', 'auth', 'unavailable', 'overloaded', 'bad_request', 'context_too_long',
                 'cancelled', 'no_route', 'no_login_for_tag')
# Transport and protocol failures have no ErrorClass of their own; the gateway being unreachable is `unavailable`.
TRANSPORT_CLASS = 'unavailable'


@dataclass(frozen=True)
class Text:
    text: str
    stop: str


@dataclass(frozen=True)
class Failure:
    cls: str
    retryable: bool
    message: str
    retry_after: int | None = None
    resets_at: int | None = None


class _UnixConnection(http.client.HTTPConnection):
    """A connection that `cut` shuts down from another thread, waking any blocked read; once cut it connects no more."""

    def __init__(self, path, timeout):
        super().__init__('localhost', timeout=timeout)
        self.path = path
        self.lock = threading.Lock()
        self.expired = False
        self.raw = None  # getresponse() hands the socket to the response and clears self.sock

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        try:
            sock.connect(self.path)
        except OSError:
            sock.close()
            raise
        with self.lock:
            if self.expired:
                sock.close()
                raise TimeoutError('deadline passed while connecting')
            self.sock = self.raw = sock

    def cut(self):
        with self.lock:
            self.expired = True
            if self.raw is not None:
                try:
                    self.raw.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass  # the response closed it as the turn ended


def request(route, prompt, max_tokens, request_id, system=None, tag=None):
    """A TurnRequest of one user text message; `tag`, the card's user tag, is the draft field of ADR-0068 item 4."""
    body = {'route': route, 'messages': [{'role': 'user', 'content': [{'type': 'text', 'text': prompt}]}],
            'max_tokens': max_tokens, 'request_id': request_id}
    if system is not None:
        body['system'] = system
    if tag is not None:
        body['tag'] = tag
    return body


def failure(error):
    """The Failure of a C4G `Error` object; a body that is not one is a protocol failure."""
    if not isinstance(error, dict) or error.get('class') not in ERROR_CLASSES or not isinstance(error.get('retryable'), bool):
        return Failure(TRANSPORT_CLASS, True, f'protocol: not a C4G Error: {json.dumps(error)[:300]}')
    return Failure(error['class'], error['retryable'], error.get('message', ''),
                   error.get('retry_after'), error.get('resets_at'))


def turn(socket_path, body, timeout):
    """Send one TurnRequest and read its stream to the single `stop` or `error`. `timeout` seconds bound the whole
    turn (connect, send and the full stream); past them the turn is a retryable Failure."""
    conn = _UnixConnection(socket_path, timeout)
    deadline = threading.Timer(timeout, conn.cut)
    deadline.start()
    try:
        outcome = _turn(conn, body)
    finally:
        deadline.cancel()
        conn.close()
    if conn.expired and isinstance(outcome, Failure):
        return Failure(TRANSPORT_CLASS, True, f'transport: the turn passed its deadline of {timeout}s')
    return outcome


def _turn(conn, body):
    try:
        payload = json.dumps(body, ensure_ascii=False).encode()
        conn.request('POST', TURN_PATH, payload, {'Content-Type': 'application/json',
                                                  'Content-Length': str(len(payload))})
        resp = conn.getresponse()
        if resp.status != 200:
            return failure(json.loads(resp.read()))
        return _stream(resp)
    except (OSError, http.client.HTTPException) as e:
        return Failure(TRANSPORT_CLASS, True, f'transport: {type(e).__name__}: {e}')
    except ValueError as e:
        return Failure(TRANSPORT_CLASS, True, f'protocol: {e}')


def _stream(resp):
    parts = []
    while True:
        line = resp.readline()
        if not line:
            return Failure(TRANSPORT_CLASS, True, 'transport: stream ended without stop or error')
        if not line.strip():
            continue
        event = json.loads(line)
        if not isinstance(event, dict):
            raise ValueError(f'stream line is not an object: {line[:200]!r}')
        kind = event.get('type')
        if kind == 'text_delta':
            parts.append(_field(event, 'text'))
        elif kind == 'stop':
            return Text(''.join(parts), _field(event, 'reason'))
        elif kind == 'error':
            return failure(event.get('error'))


def _field(event, name):
    value = event.get(name)
    if not isinstance(value, str):
        raise ValueError(f"{event.get('type')} event without a string {name}")
    return value
