#!/usr/bin/env python3
"""Fake dots client for Hive tests: `fake_dots.py STATE send TEXT` / `fake_dots.py STATE get JOB --json`.

Every call is appended to STATE/calls.jsonl. `send` prints job-<n>; STATE/send-mode `fail` exits 1,
`auth` exits 3 and `die` kills the calling dispatcher after logging. `get` exits 2 until STATE/reply.json
exists, then prints it with the polled job; STATE/get-mode `fail` exits 1, `auth` exits 3, `garbage` exits 0 with non-JSON output,
`hang` never answers (blocks until the caller's call timeout kills it).
"""
import json
import os
import signal
import sys
import time
from pathlib import Path

state = Path(sys.argv[1])
cmd, args = sys.argv[2], sys.argv[3:]
state.mkdir(parents=True, exist_ok=True)
calls = state / 'calls.jsonl'
with calls.open('a') as f:
    f.write(json.dumps({'cmd': cmd, 'args': args}, ensure_ascii=False) + '\n')


def mode(name):
    path = state / name
    return path.read_text().strip() if path.exists() else 'ok'


if cmd == 'send':
    if mode('send-mode') == 'die':
        os.kill(os.getppid(), signal.SIGKILL)
        sys.exit(9)
    if mode('send-mode') == 'fail':
        print('slack: invalid_auth', file=sys.stderr)
        sys.exit(1)
    if mode('send-mode') == 'auth':
        print('slack: invalid_auth', file=sys.stderr)
        sys.exit(3)
    sends = sum(json.loads(line)['cmd'] == 'send' for line in calls.read_text().splitlines())
    print(f'job-{sends}')
    sys.exit(0)
if cmd == 'get':
    if mode('get-mode') == 'fail':
        print('boom', file=sys.stderr)
        sys.exit(1)
    if mode('get-mode') == 'auth':
        print('slack: invalid_auth', file=sys.stderr)
        sys.exit(3)
    if mode('get-mode') == 'hang':
        time.sleep(3600)
        sys.exit(1)
    if mode('get-mode') == 'garbage':
        print('<html>bad gateway</html>')
        sys.exit(0)
    reply = state / 'reply.json'
    if not reply.exists():
        print('(no reply yet)')
        sys.exit(2)
    print(json.dumps(dict(json.loads(reply.read_text()), job=args[0], status='done'), ensure_ascii=False))
    sys.exit(0)
print(f'unsupported command {cmd}', file=sys.stderr)
sys.exit(1)
