"""Interleaved app-server/Task events must never finalize the head."""
import json
from types import SimpleNamespace

import pytest

from hive.live import Session
from hive import heads


def session(tmp_path, vendor='codex'):
    s = Session.__new__(Session)
    s.job = {'vendor': vendor}
    s.row = {'run_dir': str(tmp_path)}
    s.thread, s.turn = 'root', 'root-turn'
    s.done, s.error, s.pending = False, False, {}
    s.send = lambda msg: None
    return s


def event(s, method, thread='root', turn_id='root-turn', **params):
    s.observe({'method': method, 'params': {'threadId': thread, 'turnId': turn_id, **params}})


def test_codex_interleaved_children_and_stale_turn(tmp_path):
    s = session(tmp_path)
    for thread, turn in [('child', 'child-turn'), ('root', 'old-turn')]:
        event(s, 'turn/started', thread, turn, **{'turn': {'id': turn}})
        event(s, 'item/completed', thread, turn, item={'type': 'agentMessage', 'text': 'wrong final'})
        event(s, 'turn/completed', thread, turn, **{'turn': {'id': turn, 'status': 'failed'}})
    assert s.turn == 'root-turn' and not s.done and not s.error
    assert not (tmp_path / 'last-message.txt').exists()
    event(s, 'thread/tokenUsage/updated', 'child', tokenUsage={'total': {'outputTokens': 999}})
    assert not hasattr(s, 'usage')
    event(s, 'item/completed', item={'type': 'agentMessage', 'text': 'root final'})
    event(s, 'turn/completed', **{'turn': {'id': 'root-turn', 'status': 'completed'}})
    assert s.done and not s.error
    assert (tmp_path / 'last-message.txt').read_text() == 'root final'
    event(s, 'item/completed', 'child', item={'type': 'agentMessage', 'text': 'late child'})
    assert (tmp_path / 'last-message.txt').read_text() == 'root final'


@pytest.mark.parametrize('child_session,parent', [('child', None), ('root', 'task')])
def test_claude_task_result_and_fallback(tmp_path, child_session, parent):
    s = session(tmp_path, 'claude')
    root = {'type': 'result', 'session_id': 'root', 'result': 'root final', 'usage': {'output_tokens': 7}}
    child = {'type': 'result', 'session_id': child_session, 'parent_tool_use_id': parent,
             'result': 'child final', 'is_error': True, 'usage': {'output_tokens': 999}}
    s.observe(child)
    assert s.thread == 'root' and not s.done and not s.error
    s.observe(root)
    assert s.done and not s.error
    stream = [{'type': 'system', 'subtype': 'init', 'session_id': 'root'}, child, root, child]
    _, tout, detail, text = heads.parse_usage('claude', '\n'.join(map(json.dumps, stream)), tmp_path)
    assert tout == 7 and text == 'root final' and not detail['is_error']
