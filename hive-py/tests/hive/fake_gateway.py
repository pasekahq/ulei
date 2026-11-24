"""Scripted qb model gateway (C4G) on a unix socket: every request body is checked against the vendored TurnRequest,
kept in `requests`, and answered by the script."""
import json
import socketserver
import threading
from pathlib import Path

from schema_check import violations

CONTRACTS = Path(__file__).resolve().parents[3] / 'contracts'
C4G = json.loads((CONTRACTS / 'c4g-qb-gateway.openapi.json').read_text(encoding='utf-8'))
TURN_REQUEST = C4G['components']['schemas']['TurnRequest']
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


def rejected(status, error):
    """A flat pre-turn `Error` body under a non-200 status."""
    return 'reject', status, error


def prompt_of(request):
    return request['messages'][0]['content'][0]['text']


def follow_review(request):
    """A judge that agrees with the review: rework with the review's text after a FAIL, done otherwise."""
    marker = '## Review: VERDICT FAIL\n'
    text = prompt_of(request)
    return answer('VERDICT: rework\n' + text.split(marker, 1)[1] if marker in text else 'VERDICT: done')


class FakeGateway:
    """`script` is a callable of the request, or a list of replies taken in order."""

    def __init__(self, path, script=follow_review):
        self.path, self.script = str(path), script
        self.requests, self.invalid = [], []
        self.lock = threading.Lock()
        self.server = socketserver.ThreadingUnixStreamServer(self.path, _Handler)
        self.server.daemon_threads = True
        self.server.gateway = self
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def reply(self, method, path, request):
        with self.lock:
            self.requests.append(request)
            problems = (violations(request, TURN_REQUEST, C4G) if (method, path) == ('POST', '/v1/model/turn')
                        else [f'{method} {path}'])
            if problems:
                self.invalid.append(problems)
                return rejected(400, {'class': 'bad_request', 'retryable': False, 'message': '; '.join(problems)})
            return self.script.pop(0) if isinstance(self.script, list) else self.script(request)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        Path(self.path).unlink()


class _Handler(socketserver.StreamRequestHandler):
    def handle(self):
        method, path, _ = self.rfile.readline().decode().split()
        headers = {}
        while (line := self.rfile.readline()) not in (b'\r\n', b''):
            name, value = line.decode().split(':', 1)
            headers[name.strip().lower()] = value.strip()
        request = json.loads(self.rfile.read(int(headers['content-length'])))
        reply = self.server.gateway.reply(method, path, request)
        if reply[0] == 'reject':
            body = json.dumps(reply[2]).encode()
            self.wfile.write(f'HTTP/1.1 {reply[1]} Rejected\r\nContent-Type: application/json\r\n'
                             f'Content-Length: {len(body)}\r\nConnection: close\r\n\r\n'.encode() + body)
            return
        self.wfile.write(b'HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n'
                         b'Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n')
        for event in reply[1]:
            line = json.dumps(event).encode() + b'\n'
            self.wfile.write(f'{len(line):x}\r\n'.encode() + line + b'\r\n')
        self.wfile.write(b'0\r\n\r\n')
