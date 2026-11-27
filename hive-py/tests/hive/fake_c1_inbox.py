"""The owner inbox's C1 daemon for the hive tests: `POST /v1/publish` and `POST /v1/get` over HTTP/1.1 on a unix
socket, one request per connection, stored in a FakeInbox that accepts every C1 kind. Every request is kept in `requests` as (method, raw body)
and checked against C1's PublishRequest or GetRequest; a violation is kept in `invalid` and answered `bad_request`."""
import json
import socket
import threading
from pathlib import Path

from fake_inbox import FakeInbox
from schema_check import violations

C1 = json.loads((Path(__file__).resolve().parents[3] / 'contracts' / 'c1-event.schema.json').read_text(encoding='utf-8'))
REQUESTS = {'/v1/publish': C1['$defs']['PublishRequest'], '/v1/get': C1['$defs']['GetRequest']}
STATUS = {'bad_request': 400, 'unauthenticated': 401, 'conflict': 409, 'invalid_packet': 422}
FAKE_ERRORS = {'unauthenticated packet': 'unauthenticated', 'event id already has different content': 'conflict'}


class FakeC1Inbox:
    """Serves `inbox` (a FakeInbox) on `path`; `close` wakes the blocked accept with one connection."""

    def __init__(self, path, key):
        self.path, self.inbox = str(path), FakeInbox(key, C1['$defs']['Kind']['enum'])
        self.requests, self.invalid = [], []
        self.closing = False
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(self.path)
        self.listener.listen(16)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def published(self):
        return [json.loads(body)['packet'] for method, body in self.requests if method == '/v1/publish']

    def serve(self):
        while True:
            sock, _ = self.listener.accept()
            if self.closing:
                sock.close()
                return
            with sock, sock.makefile('rb') as rfile:
                status, answer = self.answer(rfile)
                body = json.dumps(answer).encode()
                sock.sendall(f'HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n'
                             'Connection: close\r\n\r\n'.encode() + body)

    def answer(self, rfile):
        method, path, _ = rfile.readline().decode().split()
        headers = {}
        while (line := rfile.readline()) not in (b'\r\n', b''):
            name, value = line.decode().split(':', 1)
            headers[name.strip().lower()] = value.strip()
        raw = rfile.read(int(headers['content-length']))
        self.requests.append((path, raw))
        request = json.loads(raw)
        problems = violations(request, REQUESTS[path], C1) if method == 'POST' and path in REQUESTS else [path]
        if problems:
            self.invalid.append(problems)
            return 400, {'code': 'bad_request', 'message': '; '.join(problems)}
        if path == '/v1/get':
            row = self.inbox.get(request['id'])
            return 200, {} if row is None else {'row': dict(
                row, packet=json.loads(row['packet']), status='pending', attempts=0, available=0, created=0)}
        try:
            return 200, {'id': self.inbox.publish(request['packet'])}
        except ValueError as e:
            code = FAKE_ERRORS.get(str(e), 'invalid_packet')
            return STATUS[code], {'code': code, 'message': str(e)}

    def close(self):
        self.closing = True
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as wake:
            wake.connect(self.path)
        self.thread.join()
        self.listener.close()
        Path(self.path).unlink()
