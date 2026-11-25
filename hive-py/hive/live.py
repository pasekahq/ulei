"""Persistent vendor input owned by the runner; SQLite is the durable steer inbox.

Codex uses app-server turn/steer. Claude uses streaming input and UUID-correlated
replay acknowledgements. A write alone is never proof of delivery. The dispatcher
falls back to its existing stop/handoff path if acknowledgements do not arrive.
"""
import json
import os
import queue
import select
import subprocess
import threading
import time
import uuid
from pathlib import Path

from . import store


ACTIVE_RUN_SQL = """SELECT 1 FROM runs r JOIN cards c ON c.id=r.card_id WHERE r.id=?
                    AND r.status='running' AND c.state='running' AND NOT EXISTS
                    (SELECT 1 FROM events e WHERE e.card_id=r.card_id AND e.kind='steer_stop'
                     AND json_extract(e.data, '$.run')=r.id)"""


# A Claude head whose background task ended after its turn gets a new turn with the task's notification; when no
# turn starts within this many seconds, the head is done.
WAKE_GRACE_S = 60


class ProtocolError(Exception):
    pass


class Session:
    def __init__(self, proc, conn, row, job, log, stopping):
        self.proc, self.conn, self.row, self.job = proc, conn, row, job
        self.log, self.stopping = log, stopping
        self.messages = queue.Queue()
        self.seq, self.thread, self.turn = 0, None, None
        self.done, self.error, self.disabled = False, False, False
        self.pending, self.sent = {}, set()
        self.tasks, self.woken = set(), None  # a Claude head's running background tasks; when one ended after a turn
        self.deadline = time.monotonic() + job['timeout']
        self.reader = threading.Thread(target=self.read, daemon=True)
        os.set_blocking(proc.stdin.fileno(), False)
        self.reader.start()

    def read(self):
        try:
            for line in self.proc.stdout:
                self.log.write(line)
                self.log.flush()
                try:
                    msg = json.loads(line)
                    if isinstance(msg, dict):
                        self.messages.put(msg)
                except ValueError:
                    pass  # stderr shares the log; it is not protocol input
        finally:
            self.messages.put(None)

    def send(self, msg):
        data = memoryview((json.dumps(msg, ensure_ascii=False) + '\n').encode())
        deadline = min(self.deadline, time.monotonic() + 5)
        while data:
            if self.stopping.is_set() or time.monotonic() >= deadline:
                raise ProtocolError('head is not accepting stdin')
            if not select.select([], [self.proc.stdin], [], 0.1)[1]:
                continue
            try:
                data = data[os.write(self.proc.stdin.fileno(), data):]
            except BlockingIOError:
                continue
            except OSError as e:
                raise ProtocolError(str(e)) from e

    def receive(self, timeout=0.1):
        try:
            msg = self.messages.get(timeout=timeout)
        except queue.Empty:
            return {}
        if msg is None:
            raise ProtocolError('head closed its output')
        self.observe(msg)
        return msg

    def rpc(self, method, params, timeout=10):
        self.seq += 1
        request = self.seq
        self.send({'id': request, 'method': method, 'params': params})
        deadline = min(self.deadline, time.monotonic() + timeout)
        while not self.stopping.is_set() and time.monotonic() < deadline:
            msg = self.receive()
            if msg.get('id') == request:
                if 'error' in msg:
                    raise ProtocolError(f'{method}: {msg["error"]}')
                return msg['result']
        raise ProtocolError(f'{method}: acknowledgement timed out')

    def acknowledge(self, steer, receipt):
        with store.tx(self.conn):
            # Indexed by the run's card: this runs under the write lock, where an unindexed scan of the whole
            # journal holds hive.db for minutes.
            active = self.conn.execute(ACTIVE_RUN_SQL, (self.row['id'],)).fetchone()
            if active and steer not in store.live_steers(self.conn, self.row['id']):
                store.event(self.conn, self.row['card_id'], 'steer_live', {
                    'run': self.row['id'], 'steer': steer, 'vendor': self.job['vendor'],
                    'session': self.thread, 'receipt': receipt}, actor='runner')

    def observe(self, msg):
        if self.job['vendor'] == 'claude':
            # Task/subagent events can share the stream (and even session_id).
            if msg.get('parent_tool_use_id') is not None:
                return
            session = msg.get('session_id')
            if self.thread is None and session:
                self.thread = session
            if session and session != self.thread:
                return
            if msg.get('type') == 'system' and msg.get('subtype') == 'background_tasks_changed':
                # The CLI kills its background tasks when its input closes, so a turn that ends while one runs does
                # not end the head: the task's end starts the next turn, as a steer does.
                tasks = {t.get('task_id') for t in msg.get('tasks') or []}
                if self.done and self.tasks - tasks:
                    self.done, self.woken = False, time.monotonic()
                self.tasks = tasks
                return
            if msg.get('type') == 'assistant':
                self.woken = None
            if msg.get('type') == 'user' and msg.get('uuid') in self.pending:
                self.acknowledge(self.pending.pop(msg['uuid']), msg['uuid'])
                self.done = False
            if msg.get('type') == 'result':
                self.done = True
                self.error = self.error or bool(msg.get('is_error'))
            return
        method, p = msg.get('method'), msg.get('params') or {}
        # Reject interactive requests even when they belong to a child thread.
        if method and 'id' in msg:
            self.send({'id': msg['id'], 'error': {'code': -32601, 'message': 'Hive cannot answer interactive requests'}})
        if p.get('threadId') != self.thread or self.thread is None:
            return
        if method == 'turn/started' and self.turn is None:
            self.turn = p['turn']['id']
        elif method == 'turn/completed' and p['turn']['id'] == self.turn:
            self.done = True
            self.error = p['turn']['status'] != 'completed'
        elif (method == 'item/completed' and p.get('turnId') == self.turn
              and p.get('item', {}).get('type') == 'agentMessage'):
            (Path(self.row['run_dir']) / 'last-message.txt').write_text(p['item']['text'])
        elif method == 'thread/tokenUsage/updated':
            u = p['tokenUsage']['total']
            # Keep the existing usage parser contract. Totals replace previous totals.
            self.usage = {'input_tokens': u.get('inputTokens', 0), 'output_tokens': u.get('outputTokens', 0)}

    def user_message(self, text, message_id):
        self.send({'type': 'user', 'uuid': message_id, 'session_id': self.thread or '',
                   'parent_tool_use_id': None, 'message': {'role': 'user', 'content': text}})

    def start(self):
        if self.job['vendor'] == 'claude':
            self.user_message(self.job['prompt'], str(uuid.uuid4()))
            return
        self.rpc('initialize', {'clientInfo': {'name': 'secretary_hive', 'version': '1'}}, timeout=30)
        self.send({'method': 'initialized', 'params': {}})
        denied = self.job['deny_tools']
        sandbox = 'read-only' if 'write' in denied else 'workspace-write' if denied else 'danger-full-access'
        config = {'sandbox_workspace_write.network_access': False} if 'network' in denied else {}
        result = self.rpc('thread/start', {'model': self.job['model'], 'cwd': self.job['cwd'],
                                         'approvalPolicy': 'never', 'sandbox': sandbox, 'config': config}, timeout=30)
        self.thread = result['thread']['id']
        result = self.rpc('turn/start', {'threadId': self.thread, 'effort': self.job['effort'],
                                       'input': [{'type': 'text', 'text': self.job['prompt']}]})
        self.turn = result['turn']['id']

    def deliver(self):
        if self.disabled or self.stopping.is_set():
            return
        for s in store.unseen_steers(self.conn, self.row['id']):
            if s['id'] in self.sent:
                continue
            self.sent.add(s['id'])
            text = (f"## Owner clarification (from {s['by']})\n{s['text']}\n\n"
                    'Apply this clarification and quote it verbatim in REPORT.md. Continue the same card with your full context.')
            if self.job['vendor'] == 'claude':
                key = str(uuid.uuid4())
                self.pending[key] = s['id']
                self.done = False
                self.user_message(text, key)
            else:
                result = self.rpc('turn/steer', {'threadId': self.thread, 'expectedTurnId': self.turn,
                                               'input': [{'type': 'text', 'text': text}]})
                if result.get('turnId') != self.turn:
                    raise ProtocolError('turn/steer returned a different turn')
                self.acknowledge(s['id'], result)

    def run(self):
        try:
            self.start()
            while not self.stopping.is_set():
                if time.monotonic() >= self.deadline:
                    raise subprocess.TimeoutExpired(self.job['argv'], self.job['timeout'])
                self.receive()
                try:
                    self.deliver()
                except ProtocolError as e:
                    self.fail(e)
                if self.woken is not None and time.monotonic() - self.woken >= WAKE_GRACE_S:
                    self.done, self.woken = True, None  # no turn followed the task's end
                if self.done and not self.pending and not self.tasks:
                    return 1 if self.error else 0
        except ProtocolError as e:
            self.fail(e)
            return 1
        finally:
            if hasattr(self, 'usage'):
                self.log.write((json.dumps({'type': 'turn.completed', 'usage': self.usage}) + '\n').encode())
                self.log.flush()
        return 1

    def fail(self, error):
        if not self.disabled:
            with store.tx(self.conn):
                store.event(self.conn, self.row['card_id'], 'steer_live_failed',
                            {'run': self.row['id'], 'error': str(error)}, actor='runner')
            self.disabled = True

    def close(self):
        self.proc.stdin.close()
        self.reader.join(timeout=5)
