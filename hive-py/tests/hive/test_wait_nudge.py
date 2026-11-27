"""The wait nudge: a head that runs one command for `head_wait_nudge.interval_s` gets a nudge, again every interval
while that command runs, and is never stopped for it (hive/wait_nudge.py)."""
import shutil

import pytest
from test_hive import env, live, mk, run_of, events_of, tick_until_terminal  # noqa: F401
from test_inspect import proc, unit, running, NOW, UPTIME
from test_live_steer import enable, wait_for
from test_liveness import manager
from hive import api, dispatcher, heads, store, wait_nudge


class Clock:
    def __init__(self, now):
        self.now = now

    def __call__(self):
        return self.now


def uptime(tmp_path, value):
    (tmp_path / 'proc' / 'uptime').write_text(f'{value} 0\n')


def head(tmp_path, command_pid=502, age=1300, argv=('/bin/bash', '-c', 'until false; do sleep 60; done')):
    """runner 500 → head 501 → the command it waits on, in unit hive-run-9; the command started `age` seconds ago."""
    manager(tmp_path / 'cg')
    proc(tmp_path, 500, 1, ['python', '-m', 'hive.runner'], 3000)
    proc(tmp_path, 501, 500, ['vendor-cli', '--print'], 2990)
    proc(tmp_path, command_pid, 501, list(argv), age)
    unit(tmp_path, 'hive-run-9', [500, 501, command_pid])


def setup(env, tmp_path, **kw):
    cfg, conn, _ = env
    cid = mk(conn, 'waits on a command')
    head(tmp_path, **kw)
    rid = running(conn, cid, 'hive-run-9', 500)
    conn.execute('UPDATE runs SET started_at=? WHERE id=?', (NOW - 3000, rid))
    store.event(conn, cid, 'claimed', {'run': rid, 'role': 'worker', 'round': 1, 'steers': []})
    clock = Clock(NOW)
    d = dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {}, clock=clock,
                              cgroup_root=tmp_path / 'cg', proc_root=tmp_path / 'proc')
    return cfg, conn, d, clock, cid, rid


def nudges(conn, cid):
    return [e['data'] for e in events_of(conn, cid, 'wait_nudge')]


def later(d, clock, tmp_path, seconds, up):
    """Move the dispatcher's clock and the fake uptime on by `seconds`; returns the new uptime."""
    clock.now += seconds
    up += seconds
    uptime(tmp_path, up)
    d._nudged_at = None
    return up


def test_a_head_waiting_past_the_interval_is_nudged_once_per_interval_and_never_stopped(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1100)
    assert cfg['head_wait_nudge']['interval_s'] == 1200
    assert d.nudge_waits() == 0                       # 1100 s: below the interval
    up = later(d, clock, tmp_path, 150, UPTIME)        # 1250 s
    assert d.nudge_waits() == 1
    [n] = nudges(conn, cid)
    assert (n['run'], n['pid'], n['count'], n['live']) == (rid, 502, 1, False)
    assert 'until false; do sleep 60; done' in n['command']
    assert n['text'].startswith('You have been waiting for a command for 20 minutes: ')
    assert 'until false; do sleep 60; done' in n['text']
    up = later(d, clock, tmp_path, 600, up)            # same command, half an interval after the nudge
    assert d.nudge_waits() == 0
    up = later(d, clock, tmp_path, 600, up)            # one interval after the nudge
    assert d.nudge_waits() == 1
    assert [x['count'] for x in nudges(conn, cid)] == [1, 2]
    assert 'for 40 minutes' in nudges(conn, cid)[-1]['text']
    r = run_of(conn, cid, 1)
    assert r['status'] == 'running' and api.get(conn, cid)['state'] == 'running'
    assert not events_of(conn, cid, 'steer_stop') and not events_of(conn, cid, 'steer')


def test_another_command_starts_the_count_again(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1300)
    assert d.nudge_waits() == 1
    # The head moves on: the old command is gone, a new one has run only five minutes.
    shutil.rmtree(tmp_path / 'proc' / '502')
    proc(tmp_path, 503, 501, ['sleep', '3600'], 300)
    unit(tmp_path, 'hive-run-9', [500, 501, 503])
    up = later(d, clock, tmp_path, 1200, UPTIME)       # the new command has run 1500 s: a first nudge
    assert d.nudge_waits() == 1
    last = nudges(conn, cid)[-1]
    assert (last['pid'], last['count']) == (503, 1) and last['waited_s'] == pytest.approx(1500, abs=1)
    assert 'sleep 3600' in last['text']
    up = later(d, clock, tmp_path, 60, up)
    assert d.nudge_waits() == 0


def test_a_young_command_after_a_long_one_is_not_nudged(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1300)
    assert d.nudge_waits() == 1
    shutil.rmtree(tmp_path / 'proc' / '502')
    proc(tmp_path, 504, 501, ['make', 'test'], 30)
    unit(tmp_path, 'hive-run-9', [500, 501, 504])
    clock.now += 1300                                  # an interval later the head runs a command 30 s old
    d._nudged_at = None
    assert d.nudge_waits() == 0
    assert len(nudges(conn, cid)) == 1


def test_no_command_and_interval_zero_and_the_check_period(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1300)
    cfg['head_wait_nudge'] = dict(cfg['head_wait_nudge'], interval_s=0)
    assert d.nudge_waits() == 0
    cfg['head_wait_nudge'] = dict(cfg['head_wait_nudge'], interval_s=1200)
    assert d.nudge_waits() == 1
    clock.now += 1200                                  # due again,
    d._nudged_at = clock.now - wait_nudge.CHECK_S + 1  # the last probe was a few seconds ago
    uptime(tmp_path, UPTIME + 1200)
    assert d.nudge_waits() == 0
    unit(tmp_path, 'hive-run-9', [500, 501])           # the head runs no command
    d._nudged_at = None
    assert d.nudge_waits() == 0
    assert len(nudges(conn, cid)) == 1


def test_the_nudge_runs_in_a_tick_and_only_for_running_workers(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1300)
    conn.execute("UPDATE runs SET role='reviewer' WHERE id=?", (rid,))
    assert d.nudge_waits() == 0
    conn.execute("UPDATE runs SET role='worker' WHERE id=?", (rid,))
    d._nudged_at = None
    monkey = d.reap
    d.reap = lambda: 0                                 # the fake unit's runner is no real process to reap
    try:
        assert d.tick()['nudged'] == 1
    finally:
        d.reap = monkey
    assert store.meta_get(conn, 'dispatcher_tick')[0]['nudged'] == 1


def test_a_helper_the_vendor_cli_started_with_itself_is_no_wait(env, tmp_path):
    cfg, conn, d, clock, cid, rid = setup(env, tmp_path, age=1300)
    proc(tmp_path, 505, 501, ['node', 'tool-server.js'], 2985)    # started 5 s after the head process 501
    unit(tmp_path, 'hive-run-9', [500, 501, 502, 505])
    assert d.nudge_waits() == 1
    assert nudges(conn, cid)[-1]['pid'] == 502
    shutil.rmtree(tmp_path / 'proc' / '502')
    unit(tmp_path, 'hive-run-9', [500, 501, 505])
    later(d, clock, tmp_path, 1300, UPTIME)
    assert d.nudge_waits() == 0


def test_config_defaults_and_refusals(env):
    cfg, _, _ = env
    assert wait_nudge.config({}) == wait_nudge.DEFAULTS
    assert wait_nudge.config({'head_wait_nudge': {'interval_s': 0}})['interval_s'] == 0
    custom = wait_nudge.config({'head_wait_nudge': {'text': 'Waiting {minutes} min on {command}?'}})
    assert custom['interval_s'] == 1200
    for bad in ({'interval_s': -1}, {'interval_s': True}, {'interval_s': '20m'}, {'text': ' '},
                {'text': 'waiting {hours}'}, {'text': 'waiting {0}'}):
        with pytest.raises(ValueError, match='head_wait_nudge'):
            wait_nudge.config({'head_wait_nudge': bad})


@pytest.mark.parametrize('vendor', ['codex', 'claude'])
def test_the_runner_hands_a_nudge_to_a_live_head_which_need_not_quote_it(env, live, vendor):
    cfg, conn, d = env
    enable(cfg, vendor)
    cid = mk(conn, '[ok] [hold-live] [noquote-live] nudged head', assignee=vendor, executors=heads.executors(cfg))
    d.tick()
    ws = live.ws(cid)
    wait_for(lambda: (ws / 'live-ready.json').exists())
    rid = run_of(conn, cid, 1)['id']
    text = 'You have been waiting for a command for 21 minutes: sleep 9999'
    with store.tx(conn):
        store.event(conn, cid, 'wait_nudge', {'run': rid, 'round': 1, 'pid': 1, 'command': 'sleep 9999',
                                              'waited_s': 1260, 'count': 1, 'live': True, 'text': text})
    [ack] = wait_for(lambda: events_of(conn, cid, 'wait_nudge_live'))
    assert ack['data']['run'] == rid and ack['data']['vendor'] == vendor
    assert ack['data']['nudge'] == events_of(conn, cid, 'wait_nudge')[0]['id']
    wait_for(lambda: (ws / 'live-received.txt').exists())
    assert (ws / 'live-received.txt').read_text() == text
    d.tick()
    assert run_of(conn, cid, 1)['status'] == 'running'
    assert not events_of(conn, cid, 'steer_stop') and not events_of(conn, cid, 'steer_live_failed')
    (ws / 'finish-live').touch()
    live.finish(rid)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 1 and text not in run_of(conn, cid, 1)['report']
    assert not events_of(conn, cid, 'steer_check')


def test_a_refused_nudge_is_journaled_and_disables_nothing(env, live):
    cfg, conn, d = env
    enable(cfg, 'codex')
    cid = mk(conn, '[ok] [hold-live] [reject] refused nudge', assignee='codex', executors=heads.executors(cfg))
    d.tick()
    ws = live.ws(cid)
    wait_for(lambda: (ws / 'live-ready.json').exists())
    rid = run_of(conn, cid, 1)['id']
    with store.tx(conn):
        store.event(conn, cid, 'wait_nudge', {'run': rid, 'round': 1, 'pid': 1, 'command': 'x', 'waited_s': 1300,
                                              'count': 1, 'live': True, 'text': 'still waiting?'})
    [failed] = wait_for(lambda: events_of(conn, cid, 'wait_nudge_failed'))
    assert failed['data']['run'] == rid and 'input refused' in failed['data']['error']
    d.tick()
    assert run_of(conn, cid, 1)['status'] == 'running'
    assert not events_of(conn, cid, 'steer_live_failed') and not events_of(conn, cid, 'steer_stop')
    assert not events_of(conn, cid, 'wait_nudge_live')
    d.cancel(cid, reason='test over')
