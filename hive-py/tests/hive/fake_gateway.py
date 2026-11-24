"""Scripted qb model gateway (C4G) on a unix socket: every request body is checked against the vendored TurnRequest,
kept in `requests`, and answered by the script. A gateway with `tags` takes a tagged request against the vendored
TurnRequest extended by qb's draft `tag` field and knows the draft refusal class `no_login_for_tag`."""
import json
import socket
import threading
from pathlib import Path

from schema_check import violations

CONTRACTS = Path(__file__).resolve().parents[3] / 'contracts'
C4G = json.loads((CONTRACTS / 'c4g-qb-gateway.openapi.json').read_text(encoding='utf-8'))
TURN_REQUEST = C4G['components']['schemas']['TurnRequest']
TAGGED_TURN_REQUEST = {**TURN_REQUEST, 'properties': {**TURN_REQUEST['properties'],
                                                      'tag': {'type': 'string', 'pattern': '^[a-z][a-z0-9_-]{0,63}$'}}}
FIXTURES = CONTRACTS / 'fixtures' / 'c4g'


def fixture(name, kind='valid'):
    """A vendored C4G fixture, such as `StreamEvent.error_overloaded`."""
    return json.loads((FIXTURES / kind / f'{name}.json').read_text(encoding='utf-8'))


def answer(text, reason='end'):
    """A turn that streams `text` and stops."""
    return 'stream', [fixture('StreamEvent.route'), {'type': 'text_delta', 'text': text},
                      fixture('StreamEvent.usage'), {'type': 'stop', 'reason': reason}]


def stream(*events):
    """A 200 stream of these events, ending wherever they end."""
    return 'stream', list(events)


def trickle(events, interval):
    """A 200 stream that sends one of these events every `interval` seconds."""
    return 'stream', list(events), interval


def rejected(status, error):
    """A flat pre-turn `Error` body under a non-200 status."""
    return 'reject', status, error


def no_login(tag):
    """qb's draft refusal of a tag no login is bound to."""
    return rejected(403, {'class': 'no_login_for_tag', 'retryable': False, 'message': f'no login for tag {tag}'})


def prompt_of(request):
    return request['messages'][0]['content'][0]['text']


def follow_review(request):
    """A judge that agrees with the review: rework with the review's text after a FAIL, done otherwise."""
    marker = '## Review: VERDICT FAIL\n'
    text = prompt_of(request)
    return answer('VERDICT: rework\n' + text.split(marker, 1)[1] if marker in text else 'VERDICT: done')


class FakeGateway:
    """`script` is a callable of the request, or a list of replies taken in order; without `tags` the gateway checks
    every request against the closed vendored TurnRequest. The accept loop blocks in accept() and `close` wakes it with
    one connection, so stopping costs no poll interval."""

    def __init__(self, path, script=follow_review, tags=True):
        self.path, self.script, self.tags = str(path), script, tags
        self.requests, self.invalid = [], []
        self.lock = threading.Lock()
        self.closing = False
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen(16)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while True:
            sock, _ = self.listener.accept()
            if self.closing:
                sock.close()
                return
            threading.Thread(target=self.handle, args=(sock,), daemon=True).start()

    def reply(self, method, path, request):
        with self.lock:
            self.requests.append(request)
            schema = TAGGED_TURN_REQUEST if self.tags and 'tag' in request else TURN_REQUEST
            problems = (violations(request, schema, C4G) if (method, path) == ('POST', '/v1/model/turn')
                        else [f'{method} {path}'])
            if problems:
                self.invalid.append(problems)
                return rejected(400, {'class': 'bad_request', 'retryable': False, 'message': '; '.join(problems)})
            return self.script.pop(0) if isinstance(self.script, list) else self.script(request)

    def handle(self, sock):
        with sock, sock.makefile('rb') as rfile:
            try:
                self.answer(sock, rfile)
            except (BrokenPipeError, ConnectionResetError):
                pass  # the client hung up, as at its turn deadline

    def answer(self, sock, rfile):
        method, path, _ = rfile.readline().decode().split()
        headers = {}
        while (line := rfile.readline()) not in (b'\r\n', b''):
            name, value = line.decode().split(':', 1)
            headers[name.strip().lower()] = value.strip()
        reply = self.reply(method, path, json.loads(rfile.read(int(headers['content-length']))))
        if reply[0] == 'reject':
            body = json.dumps(reply[2]).encode()
            sock.sendall(f'HTTP/1.1 {reply[1]} Rejected\r\nContent-Type: application/json\r\n'
                         f'Content-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
            return
        sock.sendall(b'HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n'
                     b'Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n')
        interval = reply[2] if len(reply) > 2 else 0
        for event in reply[1]:
            line = json.dumps(event).encode() + b'\n'
            sock.sendall(f'{len(line):x}\r\n'.encode() + line + b'\r\n')
            if interval:
                threading.Event().wait(interval)
        sock.sendall(b'0\r\n\r\n')

    def close(self):
        self.closing = True
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as wake:
            wake.connect(self.path)
        self.thread.join()
        self.listener.close()
        Path(self.path).unlink()
