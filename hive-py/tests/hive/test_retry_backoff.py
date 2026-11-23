"""Bounded retry backoff of failed worker rounds, driven by an injected clock and intercepted head launches."""
from pathlib import Path

import pytest
from test_hive import env, fake_time, kinds, mk_series, record_spawns  # noqa: F401
from hive import api, dispatcher, store

ACCEPTANCE = 'cat attempt.txt; test -s out.txt'


@pytest.fixture
def backoff(env, monkeypatch):  # noqa: F811
    cfg, conn, d = env
    cfg['retry_backoff'] = {'base_s': 60, 'max_s': 240}
    cfg['max_rounds'] = 100
    now = fake_time(monkeypatch, d)
    spawned = record_spawns(d)
    return cfg, conn, d, now, spawned


def task(conn, review=False):
    cid, _ = api.create(conn, title='[ok] backoff', goal='make out.txt', acceptance=ACCEPTANCE, step=1,
                        source='system:test', created_by='secretary', review=review)
    return cid


def latest(conn, cid, role='worker'):
    return conn.execute('SELECT * FROM runs WHERE card_id=? AND role=? ORDER BY id DESC LIMIT 1', (cid, role)).fetchone()


def finish_worker(d, conn, cid, passing=False):
    """Record the latest worker run as exited with a fresh report, as the runner would, then tick once."""
    run = latest(conn, cid)
    workdir = Path(api.card_row(conn, cid)['workdir'])
    (workdir / 'attempt.txt').write_text(f"attempt {run['round']}\n")
    if passing:
        (workdir / 'out.txt').write_text('42\n')
    with store.tx(conn):
        conn.execute("UPDATE runs SET status='exited', exit_code=0, ended_at=?, report=? WHERE id=?",
                     (d.clock(), f"RESULT: partial round {run['round']}", run['id']))
        store.event(conn, cid, 'run_report', {'run': run['id'], 'file': 'REPORT.md', 'source': 'file',
                                              'inherited': None}, actor='runner')
    d.tick()


def retries(conn, cid):
    return [e for e in api.get(conn, cid)['events'] if e['kind'] == 'retry']


def test_failed_gate_waits_base_delay_before_next_worker(backoff):
    _, conn, d, now, spawned = backoff
    cid = task(conn)
    d.tick()
    assert len(spawned) == 1
    finish_worker(d, conn, cid)
    card = api.get(conn, cid)
    assert (card['state'], card['retry_at'], card['retry_streak']) == ('queued', 1060.0, 1)
    assert retries(conn, cid)[-1]['data'] == {'round': 1, 'reason': 'gate exit 1', 'delay_s': 60,
                                              'retry_at': 1060.0, 'retry_streak': 1}
    now[0] = 1059.9
    d.tick()
    assert len(spawned) == 1 and api.get(conn, cid)['state'] == 'queued'
    now[0] = 1060
    d.tick()
    card = api.get(conn, cid)
    assert len(spawned) == 2
    assert (card['state'], card['round'], card['retry_at']) == ('running', 2, None)


def test_non_progressing_retries_double_up_to_cap_and_keep_max_rounds(backoff):
    cfg, conn, d, now, _ = backoff
    cfg['max_rounds'] = 6
    cid = task(conn)
    d.tick()
    for _ in range(5):
        finish_worker(d, conn, cid)
        now[0] = api.get(conn, cid)['retry_at']
        d.tick()
    assert [(e['data']['delay_s'], e['data']['retry_streak']) for e in retries(conn, cid)] == [
        (60, 1), (120, 2), (240, 2), (240, 2), (240, 2)]
    finish_worker(d, conn, cid)
    card = api.get(conn, cid)
    assert (card['state'], card['block_reason'], card['round']) == ('blocked', 'gate exit 1', 6)


def test_passed_gate_resets_delay_to_base(backoff):
    _, conn, d, now, _ = backoff
    cid = task(conn, review=True)
    d.tick()
    for _ in range(2):
        finish_worker(d, conn, cid)
        now[0] = api.get(conn, cid)['retry_at']
        d.tick()
    finish_worker(d, conn, cid, passing=True)
    card = api.get(conn, cid)
    assert card['state'] == 'review' and card['retry_streak'] == 0
    assert [e['data'] for e in card['events'] if e['kind'] == 'backoff_reset'] == [
        {'round': 3, 'streak': 2, 'progress': 'gate passed'}]
    review = latest(conn, cid, 'reviewer')
    with store.tx(conn):
        conn.execute("UPDATE runs SET status='exited', exit_code=0, ended_at=?, verdict='FAIL', report='VERDICT: FAIL' "
                     'WHERE id=?', (d.clock(), review['id']))
    d.tick()
    assert [e['data']['delay_s'] for e in retries(conn, cid)] == [60, 120, 60]
    assert api.get(conn, cid)['retry_streak'] == 1


def test_restarted_dispatcher_honors_persisted_delay_and_launches_once(backoff):
    cfg, conn, d, now, spawned = backoff
    cid = task(conn)
    d.tick()
    finish_worker(d, conn, cid)
    fresh = dispatcher.Dispatcher(cfg, store.connect(cfg['db']), limits_fetcher=lambda c: {}, inbox=d._inbox,
                                  clock=lambda: now[0])
    relaunched = record_spawns(fresh)
    now[0] = 1030
    fresh.tick()
    assert relaunched == [] and api.get(conn, cid)['state'] == 'queued'
    now[0] = 1060
    fresh.tick()
    fresh.tick()
    d.tick()
    assert len(relaunched) == 1 and len(spawned) == 1
    assert [r['round'] for r in api.get(conn, cid)['runs']] == [1, 2]


def test_delayed_card_keeps_feedback_and_steer_for_next_round(backoff):
    _, conn, d, now, _ = backoff
    cid = task(conn)
    d.tick()
    finish_worker(d, conn, cid)
    steer = api.steer(conn, cid, 'write the answer in out.txt', actor='owner')
    now[0] = 1030
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'queued' and 'exited 1' in card['feedback']
    assert [s['delivery'] for s in api.steer_deliveries(conn, cid)] == ['pending']
    now[0] = 1060
    d.tick()
    run = latest(conn, cid)
    assert run['round'] == 2
    assert 'exited 1' in run['prompt'] and 'write the answer in out.txt' in run['prompt']
    claim = [e for e in api.get(conn, cid)['events'] if e['kind'] == 'claimed'][-1]
    assert claim['data']['steers'] == [steer]


def test_series_instance_backs_off_without_breaking_series(backoff):
    _, conn, d, now, spawned = backoff
    sid = mk_series(conn, every=60, max_runs=3)
    d.tick()
    [iid] = [i['id'] for i in api.get(conn, sid)['instances']]
    assert len(spawned) == 1
    finish_worker(d, conn, iid)
    assert api.get(conn, iid)['retry_at'] == 1060.0
    now[0] = 1060
    d.tick()
    series = api.get(conn, sid)
    instance = api.get(conn, iid)
    assert [(i['id'], i['period']) for i in series['instances']] == [(iid, 1)]
    assert [e['data']['reason'] for e in series['events'] if e['kind'] == 'periods_skipped'] == ['overlap']
    assert instance['series'] == {'id': sid, 'period': 1}
    assert (instance['state'], instance['round'], len(spawned)) == ('running', 2, 2)
    finish_worker(d, conn, iid)
    assert [e['data']['delay_s'] for e in retries(conn, iid)] == [60, 120]
    assert 'loop_stopped' not in kinds(conn, iid)


def test_retry_backoff_config_refuses_cap_below_base(tmp_path):
    config = tmp_path / 'config.toml'
    config.write_text('db = "x"\n[ladder.1]\n[retry_backoff]\nbase_s = 300\nmax_s = 60\n')
    with pytest.raises(ValueError, match='max_s 60 is below base_s 300'):
        store.load_config(config)
