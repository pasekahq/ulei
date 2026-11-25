"""Real runner/dispatcher processes with deterministic vendor protocol peers."""
import json
from pathlib import Path
import sys
import time

import pytest

from test_hive import env, live, mk, run_of, events_of, workers, tick_until_terminal, deliveries, run_until
from hive import api, heads, live as live_session, store

FAKE = Path(__file__).with_name('fake_live_head.py')


def enable(cfg, vendor):
    cfg['vendors'][vendor]['binary'] = [f'{sys.executable} {FAKE}']
    cfg['vendors'][vendor]['live_input'] = True
    cfg['live_steer_grace_s'] = 2


def wait_for(check, timeout=15):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        if value := check():
            return value
        time.sleep(.02)
    raise AssertionError('timed out waiting for live input evidence')


@pytest.mark.parametrize('vendor', ['codex', 'claude'])
def test_live_delivery_preserves_run_process_memory_and_acknowledges_report(env, live, vendor):
    cfg, conn, d = env
    enable(cfg, vendor)
    cid = mk(conn, '[ok] [hold-live] live steer', assignee=vendor, executors=heads.executors(cfg))
    d.tick()
    ws = live.ws(cid)
    wait_for(lambda: (ws / 'live-ready.json').exists())
    memory = json.loads((ws / 'live-ready.json').read_text())
    rid = run_of(conn, cid, 1)['id']
    texts = ['Сохрани `42`\n  и добавь заголовок', 'second clarification']
    ids = [api.steer(conn, cid, text, actor='owner') for text in texts]
    d.tick()  # an immediate tick must not stop a head before its acknowledgement
    wait_for(lambda: len(events_of(conn, cid, 'steer_live')) == 2)
    assert store.unseen_steers(conn, rid) == []
    assert deliveries(conn, cid) == [('delivered-live', rid)] * 2
    d.tick()
    assert run_of(conn, cid, 1)['status'] == 'running'
    (ws / 'finish-live').touch()
    live.finish(rid)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert len(workers(c)) == 1 and c['round'] == 1
    assert workers(c)[0]['id'] == rid
    assert f"pid={memory['pid']} memory={memory['memory']}" in workers(c)[0]['report']
    assert all(text in workers(c)[0]['report'] for text in texts)
    assert not events_of(conn, cid, 'steer_stop')
    assert [s['method'] for s in c['steers']] == ['delivered-live'] * 2
    assert deliveries(conn, cid) == [('acknowledged', rid)] * 2
    assert events_of(conn, cid, 'steer_check')[-1]['data']['acknowledged'] == ids
    assert all(e['data']['session'] == memory['session'] for e in events_of(conn, cid, 'steer_live'))
    if vendor == 'codex':
        assert workers(c)[0]['tokens_in'] == 12 and workers(c)[0]['tokens_out'] == 4


@pytest.mark.parametrize('vendor', ['codex', 'claude'])
@pytest.mark.parametrize('failure', ['reject', 'silent', 'dead'])
def test_unaccepted_input_falls_back_and_exposes_restart(env, live, vendor, failure):
    cfg, conn, d = env
    enable(cfg, vendor)
    cfg['live_steer_grace_s'] = .25
    cid = mk(conn, f'[ok] [{failure}] fallback', assignee=vendor, executors=heads.executors(cfg))
    d.tick()
    wait_for(lambda: (live.ws(cid) / 'live-ready.json').exists())
    rid = run_of(conn, cid, 1)['id']
    sid = api.steer(conn, cid, 'required clarification', actor='owner')
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert len(workers(c)) == 2 and workers(c)[1]['id'] != rid
    assert not events_of(conn, cid, 'steer_live')
    assert events_of(conn, cid, 'steer_restart')[0]['data']['steers'] == [sid]
    assert c['steers'][0]['method'] == 'restarted' and c['steers'][0]['reason']
    assert c['steers'][0]['delivery'] == 'acknowledged'
    assert 'required clarification' in workers(c)[1]['report']


@pytest.mark.parametrize('denied', [[], ['write'], ['network'], ['browser']])
def test_codex_live_command_keeps_search_policy(env, denied):
    cfg, _, _ = env
    argv = heads.command(cfg, 'codex', 'model', 'high', '/tmp', '/tmp/run', denied, live=True)
    assert 'app-server' in argv
    assert ('web_search="disabled"' if set(denied) & {'network', 'browser'} else 'web_search="live"') in argv


@pytest.mark.parametrize('vendor', ['codex', 'claude'])
def test_live_input_still_requires_report_quote(env, live, vendor):
    cfg, conn, d = env
    enable(cfg, vendor)
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] [noquote-live]', assignee=vendor, executors=heads.executors(cfg))
    d.tick()
    wait_for(lambda: (live.ws(cid) / 'live-ready.json').exists())
    sid = api.steer(conn, cid, 'must be quoted', actor='owner')
    assert run_until(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    assert c['steers'][0]['method'] == 'delivered-live'
    assert c['steers'][0]['delivery'] == 'unacknowledged'
    assert events_of(conn, cid, 'steer_check')[-1]['data']['missing'] == [sid]


def test_ack_racing_dispatch_lock_does_not_restart_newer_input(env, monkeypatch):
    import contextlib
    cfg, conn, d = env
    enable(cfg, 'codex')
    d.spawn = lambda *args: None
    cid = mk(conn, '[ok] race', assignee='codex', executors=heads.executors(cfg))
    d.tick()
    rid = run_of(conn, cid, 1)['id']
    first = api.steer(conn, cid, 'old input', actor='owner')
    conn.execute('UPDATE events SET at=? WHERE id=?', (time.time() - 100, first))
    second = api.steer(conn, cid, 'new input', actor='owner')
    original_tx = store.tx

    @contextlib.contextmanager
    def acknowledge_before_lock(c):
        with original_tx(c):
            store.event(c, cid, 'steer_live', {'run': rid, 'steer': first})
        with original_tx(c):
            yield

    monkeypatch.setattr(store, 'tx', acknowledge_before_lock)
    d.stop_for_steers()
    assert not events_of(conn, cid, 'steer_stop')
    assert [s['id'] for s in store.unseen_steers(conn, rid)] == [second]


def test_live_steer_default_grace_waits_90_seconds_before_restart(env):
    """A head inside a long tool call reads input only after it; 45 s restarts it mid-Bash."""
    from hive import dispatcher
    cfg, conn, d = env
    enable(cfg, 'codex')
    cfg.pop('live_steer_grace_s')
    assert dispatcher.LIVE_STEER_GRACE_S == 90
    d.spawn = lambda *args: None
    killed = []
    d.kill = lambda r: killed.append(r['id'])
    cid = mk(conn, '[ok] long tool call', assignee='codex', executors=heads.executors(cfg))
    d.tick()
    rid = run_of(conn, cid, 1)['id']
    conn.execute("UPDATE runs SET status='running' WHERE id=?", (rid,))
    sid = api.steer(conn, cid, 'clarification during a long Bash', actor='owner')
    sent = conn.execute('SELECT at FROM events WHERE id=?', (sid,)).fetchone()['at']
    now = [sent + 60]
    d.clock = lambda: now[0]
    assert d.stop_for_steers() == 0 and not killed
    assert not events_of(conn, cid, 'steer_restart')
    now[0] = sent + 91
    assert d.stop_for_steers() == 1 and killed == [rid]
    assert events_of(conn, cid, 'steer_restart')[0]['data']['reason'] == 'live input acknowledgement timed out'


@pytest.mark.parametrize('vendor', ['codex', 'claude'])
def test_live_head_cancellation_and_timeout_clean_up_processes(env, live, vendor):
    cfg, conn, d = env
    enable(cfg, vendor)
    cid = mk(conn, '[ok] cancelled live head', assignee=vendor, executors=heads.executors(cfg))
    d.tick()
    wait_for(lambda: (live.ws(cid) / 'live-ready.json').exists())
    rid = run_of(conn, cid, 1)['id']
    d.cancel(cid, actor='secretary', reason='cancel protocol peer')
    live.finish(rid)
    assert api.get(conn, cid)['state'] == 'cancelled'
    assert not events_of(conn, cid, 'steer_live')

    cfg['timeouts'][1] = 1
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] timed out live head', assignee=vendor, executors=heads.executors(cfg))
    assert run_until(d, conn, cid) == 'blocked'
    row = run_of(conn, cid, 1)
    live.finish(row['id'])
    assert row['status'] == 'timeout'


def test_claude_head_is_not_done_while_a_background_task_runs(env, live):
    """The CLI kills its background tasks when the runner closes its input: a turn that ends while one runs leaves
    the head open, and the turn the task's end starts finishes the work."""
    cfg, conn, d = env
    enable(cfg, 'claude')
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] [background]', assignee='claude', executors=heads.executors(cfg))
    d.tick()
    rid = run_of(conn, cid, 1)['id']
    live.finish(rid)
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert (live.ws(cid) / 'out.txt').read_text() == '42\n'


def claude_session():
    s = object.__new__(live_session.Session)
    s.job, s.thread, s.pending = {'vendor': 'claude'}, 'session', {}
    s.done, s.error, s.tasks, s.woken = False, False, set(), None
    return s


def tasks_changed(*ids):
    return {'type': 'system', 'subtype': 'background_tasks_changed', 'session_id': 'session',
            'tasks': [{'task_id': i, 'task_type': 'local_bash'} for i in ids]}


def test_claude_session_waits_for_the_turn_a_finished_task_starts():
    s = claude_session()
    s.observe(tasks_changed('bg-1'))
    s.observe({'type': 'result', 'session_id': 'session'})
    assert s.done and s.tasks == {'bg-1'}
    s.observe(tasks_changed())
    assert not s.done and s.woken is not None and not s.tasks
    s.observe({'type': 'assistant', 'session_id': 'session', 'message': {'content': []}})
    assert s.woken is None
    s.observe({'type': 'result', 'session_id': 'session'})
    assert s.done


def test_claude_session_task_ending_inside_a_turn_needs_no_new_turn():
    s = claude_session()
    s.observe(tasks_changed('bg-1'))
    s.observe(tasks_changed())
    s.observe({'type': 'result', 'session_id': 'session'})
    assert s.done and s.woken is None and not s.tasks
    s.observe(tasks_changed('bg-2', 'bg-3'))  # a child's task list of another session is not the head's
    s.observe({**tasks_changed(), 'session_id': 'child'})
    assert s.tasks == {'bg-2', 'bg-3'}
