#!/usr/bin/env python3
"""Protocol peer, no models: memory-only secret proves the original process continued."""
import json
import os
from pathlib import Path
import runpy
import sys
import uuid
import threading
import time

codex = 'app-server' in sys.argv
if not codex and 'stream-json' not in sys.argv:
    runpy.run_path(str(Path(__file__).with_name('fake_head.py')), run_name='__main__')
    sys.exit()

def emit(msg):
    print(json.dumps(msg), flush=True)

secret = str(uuid.uuid4())
thread, turn = 'thread-' + secret, 'turn-' + secret
prompt = ''

def finish(text):
    Path('REPORT.md').write_text(f'pid={os.getpid()} memory={secret}\n' + ('' if '[noquote-live]' in prompt else text))
    Path('out.txt').write_text('42\n')
    if codex:
        emit({'method': 'thread/tokenUsage/updated', 'params': {'threadId': thread, 'tokenUsage': {'total': {'inputTokens': 12, 'outputTokens': 4}}}})
        emit({'method': 'turn/completed', 'params': {'threadId': thread, 'turn': {'id': turn, 'status': 'completed'}}})
    else:
        emit({'type': 'result', 'is_error': False, 'usage': {'input_tokens': 12, 'output_tokens': 4}, 'session_id': thread})

for line in sys.stdin:
    msg = json.loads(line)
    method, params = msg.get('method'), msg.get('params', {})
    if method == 'initialize':
        emit({'id': msg['id'], 'result': {}})
    elif method == 'thread/start':
        emit({'id': msg['id'], 'result': {'thread': {'id': thread}}})
    elif method == 'turn/start' or msg.get('type') == 'user' and not prompt:
        prompt = params['input'][0]['text'] if codex else msg['message']['content']
        # The test polls for existence, so the file must appear complete.
        Path('live-ready.tmp').write_text(json.dumps({'pid': os.getpid(), 'memory': secret, 'session': thread, 'turn': turn}))
        os.replace('live-ready.tmp', 'live-ready.json')
        if codex:
            emit({'method': 'turn/started', 'params': {'threadId': thread, 'turn': {'id': turn}}})
            emit({'id': msg['id'], 'result': {'turn': {'id': turn}}})
        else:
            emit({'type': 'system', 'subtype': 'init', 'session_id': thread})
        # A child finishes while the head remains alive awaiting a steer.
        if codex:
            emit({'method': 'turn/started', 'params': {'threadId': 'child', 'turn': {'id': 'child-turn'}}})
            emit({'method': 'item/completed', 'params': {'threadId': 'child', 'turnId': 'child-turn',
                  'item': {'type': 'agentMessage', 'text': 'child final'}}})
            emit({'method': 'turn/completed', 'params': {'threadId': 'child', 'turn': {'id': 'child-turn', 'status': 'completed'}}})
        else:
            emit({'type': 'result', 'session_id': 'child', 'parent_tool_use_id': 'task', 'result': 'child final'})
        if '## Owner clarifications' in prompt:
            finish(prompt)
    elif method == 'turn/steer' or msg.get('type') == 'user':
        if '[dead]' in prompt:
            sys.exit(9)
        if '[reject]' in prompt:
            if codex:
                emit({'id': msg['id'], 'error': {'message': 'input refused'}})
            continue
        if '[silent]' in prompt:
            continue
        text = params['input'][0]['text'] if codex else msg['message']['content']
        if codex:
            assert params['expectedTurnId'] == turn and params['threadId'] == thread
            emit({'id': msg['id'], 'result': {'turnId': turn}})
        else:
            emit({'type': 'user', 'uuid': msg['uuid'], 'message': msg['message'], 'session_id': thread})
        if not Path('live-received.txt').exists():
            Path('live-received.txt').write_text(text)
        else:
            with Path('live-received.txt').open('a') as f:
                f.write(text)
        if '[hold-live]' not in prompt:
            finish(Path('live-received.txt').read_text())
        elif not globals().get('finisher'):
            def finish_when_released():
                while not Path('finish-live').exists():
                    time.sleep(.02)
                finish(Path('live-received.txt').read_text())
            finisher = threading.Thread(target=finish_when_released, daemon=True)
            finisher.start()
