import itertools
import json
import re
import subprocess
from pathlib import Path
import tomllib

import pytest
from test_hive import env, hive_cli, mk, REAL_SCHEDULE_GATE
from test_resources import enable, healthy
from hive import api, dispatcher, resources, store, watch


def test_global_order_and_watchdog_across_pools(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch)
    cfg['resources']['priority_sources'] = ['cron:watchdog']
    cfg['concurrency'] = 2
    cfg['script_concurrency'] = 2
    script = tmp_path / 'tick.sh'
    script.write_text('true')
    cron = api.create(conn, title='ordinary', goal='g', step=1, source='cron:ordinary',
                      created_by='cron', card_class='series_run', script=str(script))[0]
    watchdog = api.create(conn, title='watchdog', goal='g', step=1, source='cron:watchdog',
                          created_by='cron', card_class='series_run', script=str(script))[0]
    follow = mk(conn, 'follow', card_class='followup')
    owner = mk(conn, 'owner', card_class='owner_task')
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert [r[0] for r in conn.execute('SELECT card_id FROM runs ORDER BY id')] == [owner, follow]
    conn.execute("UPDATE runs SET status='ok',handled=1")
    conn.execute("UPDATE cards SET state='done' WHERE id IN (?,?)", (owner, follow))
    monkeypatch.setattr(resources, 'sample', lambda **_: healthy(cpu=[100, 0, 100, 1000, 0, 0, 0, 0]))
    d.tick()
    assert [r[0] for r in conn.execute('SELECT card_id FROM runs ORDER BY id')][-2:] == [watchdog, cron]


def test_series_coalesces_waiters_quietly_preserving_running(env):
    cfg, conn, d = env
    sid = api.create(conn, title='series', goal='g', step=1, source='cron:test', created_by='cron',
                     acceptance='true', kind='recurring', every=60, max_runs=10, overlap='queue')[0]
    s = api.card_row(conn, sid)
    start = api.period_start(s, 1)
    d.advance_series(start)
    first = store.instances(conn, sid)[0]['id']
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (first,))
    d.advance_series(start + 60)
    second = store.instances(conn, sid)[-1]['id']
    d.advance_series(start + 120)
    old = api.card_row(conn, second)
    assert old['state'] == 'cancelled' and old['result'] == 'superseded'
    assert api.card_row(conn, first)['state'] == 'running'
    latest = store.instances(conn, sid)[-1]
    assert store.blockers(conn, latest['id'])[0]['id'] == first
    assert not conn.execute('SELECT 1 FROM outbox WHERE card_id=?', (second,)).fetchone()
    assert conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='cancelled'", (second,)).fetchone()


def test_lease_expired_without_any_run_is_starved(env):
    _, conn, _ = env
    cid = mk(conn, 'waiting')
    assert watch._outcome(conn, api.card_row(conn, cid), True) == 'starved'
    assert watch._outcome(conn, api.card_row(conn, cid), False) == 'active'


def test_measured_memory_budget():
    cfg = tomllib.loads((Path(__file__).resolve().parents[2] / 'config.example.toml').read_text())['resources']
    assert cfg['reserve_mb'] == 1024 and cfg['per_run_mb'] == 512
    assert resources.evaluate(healthy() | {'available_mb': 2048}, {'cpu': [0]*8}, cfg, 100, 768)[0] == 2
    assert resources.evaluate(healthy() | {'available_mb': 1535}, {'cpu': [0]*8}, cfg, 100, 768)[0] == 0


def test_queued_period_superseded_without_touching_other_series(env):
    _, conn, d = env
    ids = [api.create(conn, title=name, goal='g', acceptance='true', step=1, source='cron:test',
                      created_by='cron', kind='recurring', every=60, max_runs=5)[0]
           for name in ('replace', 'other')]
    start = max(api.period_start(api.card_row(conn, sid), 1) for sid in ids)
    d.advance_series(start)
    old, other = [store.instances(conn, sid)[0]['id'] for sid in ids]
    conn.execute("UPDATE cards SET state='queued' WHERE id IN (?,?)", (old, other))
    with store.tx(conn):
        s = api.card_row(conn, ids[0])
        d.run_period(s, 2, api.card_row(conn, old), start + 60)
    assert api.card_row(conn, old)['result'] == 'superseded'
    assert api.card_row(conn, other)['state'] == 'queued'
    latest = store.instances(conn, ids[0])[-1]['id']
    assert not store.blockers(conn, latest)
    assert not conn.execute('SELECT 1 FROM outbox WHERE card_id=?', (old,)).fetchone()


def test_gate_releasing_mid_launch_cannot_let_background_overtake_watchdog(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    cfg['resources'] = {'priority_sources': ['cron:watchdog']}
    script = tmp_path / 'tick.sh'
    script.write_text('true')
    watcher = api.create(conn, title='watchdog', goal='g', step=1, source='cron:watchdog',
                         created_by='cron', card_class='series_run', script=str(script))[0]
    background = mk(conn, 'background')
    conn.execute("UPDATE cards SET state='queued'")
    calls = []
    def gate_finishes(urgent=False):
        calls.append(1)
        return len(calls) > 1
    monkeypatch.setattr(d, 'resource_room', gate_finishes)
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    assert d.launch() == []
    assert len(calls) == 1
    assert api.card_row(conn, background)['state'] == 'queued'
    runs = d.launch()
    assert conn.execute('SELECT card_id FROM runs WHERE id=?', (runs[0],)).fetchone()[0] == watcher


def test_priority_source_and_its_conditional_child_share_one_class_ahead_of_background(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    enable(cfg, conn, monkeypatch)
    cfg['resources']['max_local_runs'] = 1
    cfg['resources']['priority_sources'] = ['cron:watchdog']
    script = tmp_path / 'tick.sh'
    script.write_text('true')
    ordinary = mk(conn, 'ordinary')
    child = api.create(conn, title='watch child retry', goal='g', acceptance='true', step=1,
                       source='cron:watchdog:conditional', created_by='cron', card_class='series_run')[0]
    conn.execute('UPDATE cards SET round=1 WHERE id=?', (child,))
    tick = api.create(conn, title='watch tick', goal='g', step=1, source='cron:watchdog',
                      created_by='cron', card_class='series_run', script=str(script))[0]
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert [r[0] for r in conn.execute('SELECT card_id FROM runs')] == [child]
    conn.execute("UPDATE runs SET status='exited',handled=1")
    conn.execute("UPDATE cards SET state='done' WHERE id=?", (child,))
    monkeypatch.setattr(resources, 'sample', lambda **_: healthy(cpu=[100,0,100,1000,0,0,0,0]))
    d.tick()
    assert [r[0] for r in conn.execute('SELECT card_id FROM runs ORDER BY id')] == [child, tick]
    assert api.card_row(conn, ordinary)['state'] == 'queued'


# Creation order differs from the class order, so FIFO alone cannot pass these tests.
CLASS_CASES = [
    ('background a', dict(source='system:test')),
    ('followup', dict(source='system:test', card_class='followup')),
    ('owner step 4', dict(source='msg:1', card_class='owner_task', step=4)),
    ('background b', dict(source='system:test')),
    ('owner step 1', dict(source='msg:2', card_class='owner_task', step=1)),
    ('watchdog', dict(source='cron:000000000001', card_class='series_run')),
    ('watchdog child', dict(source='cron:000000000002:conditional', card_class='series_run')),
]
LAUNCH_ORDER = ['watchdog', 'watchdog child', 'owner step 4', 'owner step 1', 'followup', 'background a',
                'background b']
CLASS_OF = {'owner step 1': 'owner', 'watchdog': 'urgent', 'watchdog child': 'urgent', 'owner step 4': 'owner',
            'followup': 'followup', 'background a': 'background', 'background b': 'background'}
URGENT_OWNER = dict(source='msg:9', card_class='owner_task', step=3, urgent=True)


def make_pool_card(pool, cfg, conn, tmp_path, title, kw):
    kw = {'step': 1, 'acceptance': 'true', **kw}
    if pool == 'script':
        script = tmp_path / 'job.sh'
        script.write_text('true')
        kw['script'] = str(script)
    if pool == 'external':
        kw.update(executor='dots', cfg=cfg)
    return api.create(conn, title=title, goal='g', created_by='secretary', **kw)[0]


@pytest.mark.parametrize('pool', ['head', 'script', 'external'])
def test_classes_launch_in_one_order_in_every_pool(env, tmp_path, monkeypatch, pool):
    cfg, conn, d = env
    cfg['concurrency'] = cfg['script_concurrency'] = cfg['executors']['dots']['max_parallel'] = 10
    ids = {}
    for at, (title, kw) in enumerate(CLASS_CASES, 1):
        ids[title] = make_pool_card(pool, cfg, conn, tmp_path, title, kw)
        conn.execute('UPDATE cards SET created_at=? WHERE id=?', (at, ids[title]))
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    monkeypatch.setattr(d, 'external', lambda name, *args: subprocess.CompletedProcess(args, 0, 'job-1\n', ''))
    d.tick()
    title_of = {cid: title for title, cid in ids.items()}
    assert [title_of[r[0]] for r in conn.execute('SELECT card_id FROM runs ORDER BY id')] == LAUNCH_ORDER
    claimed = {title_of[r[0]]: json.loads(r[1])['admission_class']
               for r in conn.execute("SELECT card_id, data FROM events WHERE kind='claimed'")}
    assert claimed == CLASS_OF


def test_urgent_owner_card_at_step_3_launches_first(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    cfg['concurrency'] = 10
    ids = {title: make_pool_card('head', cfg, conn, tmp_path, title, kw) for title, kw in CLASS_CASES}
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent step 3', URGENT_OWNER)
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert next(iter(launched(conn))) == urgent
    assert claim(conn, urgent)['admission_class'] == 'urgent'
    assert api.card_row(conn, urgent)['urgent'] == 1
    assert all(api.card_row(conn, cid)['urgent'] == 0 for cid in ids.values())


def test_within_a_class_review_precedes_higher_priority_which_precedes_older(env, monkeypatch):
    cfg, conn, d = env
    cfg['concurrency'] = 10
    old, high, reviewed = (mk(conn, title) for title in ('old', 'high', 'reviewed'))
    conn.execute('UPDATE cards SET created_at=1 WHERE id=?', (old,))
    conn.execute('UPDATE cards SET created_at=2, priority=5 WHERE id=?', (high,))
    conn.execute("UPDATE cards SET created_at=3, state='review', round=1 WHERE id=?", (reviewed,))
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    assert [r[0] for r in conn.execute('SELECT card_id FROM runs ORDER BY id')] == [reviewed, high, old]


def test_admission_section_defaults_and_checked_in_values(env, tmp_path, monkeypatch, capsys):
    cfg, conn, _ = env
    checked_in = store.load_config(Path(__file__).resolve().parents[2] / 'config.example.toml')
    for title, kw in [*CLASS_CASES, ('owner urgent step 3', URGENT_OWNER)]:
        cid = make_pool_card('head', cfg, conn, tmp_path, title, kw)
        conn.execute("UPDATE cards SET state='queued' WHERE id=?", (cid,))
        card = api.card_row(conn, cid)
        assert api.admission_class(card, cfg) == api.admission_class(card, checked_in) == CLASS_OF.get(title, 'urgent')
    assert hive_cli(monkeypatch, cfg, 'list') == 0
    lines = capsys.readouterr().out.splitlines()
    for title, cls in CLASS_OF.items():
        assert cls in next(line for line in lines if line.endswith(title)).split()
    assert hive_cli(monkeypatch, cfg, 'list', '--json') == 0
    listed = {r['title']: r['admission_class'] for r in json.loads(capsys.readouterr().out)}
    assert listed == {**CLASS_OF, 'owner urgent step 3': 'urgent'}
    config = Path(cfg['_path'])
    for section in ('urgent_sources = "cron:x"', 'urgent_floor_mb = -1', 'urgent_floor_mb = true'):
        bad = tmp_path / 'bad.toml'
        text = config.read_text()
        if section.startswith('urgent_sources'):
            text = re.sub(r'urgent_sources = .*\n', '', text)
        bad.write_text(text.replace('[admission]\n', f'[admission]\n{section}\n'))
        with pytest.raises(ValueError, match='admission'):
            store.load_config(bad)


@pytest.mark.parametrize('kw', [dict(card_class='system'), dict(card_class='followup'), dict(kind='epic'),
                                dict(reminder='synthetic reminder', not_before=4102444800.0),
                                dict(urgent='yes')])
def test_urgent_belongs_to_an_owner_task_run_by_a_head(env, kw):
    _, conn, _ = env
    kw = {'card_class': 'owner_task', 'urgent': True, **kw}
    with pytest.raises(api.CardError, match='urgent'):
        api.create(conn, title='urgent', goal='g', acceptance='true', step=1, source='msg:7', created_by='secretary', **kw)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 0


def test_cli_creates_an_urgent_owner_card_and_refuses_other_classes(env, monkeypatch, capsys):
    cfg, conn, _ = env
    base = ('create', '--title', 'now', '--goal', 'g', '--acceptance', 'true', '--step', '3', '--source', 'msg:8',
            '--created-by', 'secretary', '--json')
    assert hive_cli(monkeypatch, cfg, *base, '--kind', 'owner_task', '--urgent') == 0
    created = json.loads(capsys.readouterr().out)
    assert api.card_row(conn, created['id'])['urgent'] == 1
    assert hive_cli(monkeypatch, cfg, *base, '--kind', 'system', '--urgent') == 2
    assert 'urgent applies only to an owner_task card' in capsys.readouterr().err
    assert hive_cli(monkeypatch, cfg, *base, '--kind', 'owner_task') == 0
    assert api.card_row(conn, json.loads(capsys.readouterr().out)['id'])['urgent'] == 0


WATCHDOG = dict(source='cron:000000000001', card_class='series_run')


def no_spawn(env, monkeypatch):
    cfg, conn, d = env
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    return cfg, conn, d


def shared_pool(env, monkeypatch, **sample):
    """A host with a fresh CPU sample every tick and memory for many runs, shaped by `sample`; concurrency 5 and a
    shared pool as wide."""
    cfg, conn, d = no_spawn(env, monkeypatch)
    enable(cfg, conn, monkeypatch)
    cfg['resources']['max_local_runs'] = cfg['concurrency'] = 5
    cfg['script_concurrency'] = 2
    idle = itertools.count(1000, 100)
    monkeypatch.setattr(resources, 'sample', lambda **_: {**healthy(cpu=[100, 0, 100, next(idle), 0, 0, 0, 0]),
                                                          'available_mb': 64000, **sample})
    return cfg, conn, d


def launched(conn):
    return {r[0]: r[1] for r in conn.execute('SELECT card_id, id FROM runs ORDER BY id')}


def claim(conn, cid):
    return json.loads(conn.execute("SELECT data FROM events WHERE card_id=? AND kind='claimed'", (cid,)).fetchone()[0])


def assert_still_active(conn, run_ids):
    rows = conn.execute(f"SELECT status, ended_at FROM runs WHERE id IN ({','.join('?' * len(run_ids))})",
                        list(run_ids)).fetchall()
    assert len(rows) == len(run_ids)
    assert all(r['status'] == 'starting' and r['ended_at'] is None for r in rows)


def gate_state(conn):
    return store.meta_get(conn, 'resource_gate')[0]


def close_gate(monkeypatch, **sample):
    monkeypatch.setattr(resources, 'sample', lambda **_: {**healthy(), 'available_mb': 64000, 'some': 90, **sample})


def test_urgent_owner_card_runs_sixth_above_concurrency_5_while_the_psi_gate_is_closed(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    background = [mk(conn, f'background {i}') for i in range(5)]
    d.tick()
    assert list(launched(conn)) == background
    held = list(launched(conn).values())
    close_gate(monkeypatch)
    d.clock = lambda: 1000
    owner = mk(conn, 'owner step 3', card_class='owner_task', step=3)
    later = mk(conn, 'background late')
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent step 3', URGENT_OWNER)
    d.tick()
    assert list(launched(conn))[5:] == [urgent]
    assert claim(conn, urgent)['admission_class'] == 'urgent' and claim(conn, urgent)['bypass'] == 'pressure'
    assert gate_state(conn)['last_urgent'] == {'card': urgent, 'run': launched(conn)[urgent], 'bypass': 'pressure',
                                               'bypassed': 'io PSI some', 'at': 1000}
    assert api.card_row(conn, owner)['state'] == api.card_row(conn, later)['state'] == 'queued'
    assert_still_active(conn, held)


def test_a_third_urgent_card_waits_until_one_of_two_ends(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    cfg['resources']['max_local_runs'] = cfg['concurrency'] = 1
    background = [mk(conn, f'background {i}') for i in range(2)]
    urgent = [make_pool_card('head', cfg, conn, tmp_path, f'owner urgent {i}', URGENT_OWNER) for i in range(3)]
    d.tick()
    d.tick()
    assert list(launched(conn)) == [*urgent[:2], background[0]]
    assert [claim(conn, cid)['admission_class'] for cid in urgent[:2]] == ['urgent', 'urgent']
    assert api.card_row(conn, urgent[2])['state'] == api.card_row(conn, background[1])['state'] == 'queued'
    conn.execute("UPDATE runs SET status='exited', handled=1 WHERE card_id=?", (urgent[0],))
    conn.execute("UPDATE cards SET state='done' WHERE id=?", (urgent[0],))
    d.tick()
    assert list(launched(conn))[3:] == [urgent[2]]
    assert api.card_row(conn, background[1])['state'] == 'queued'


def test_urgent_runs_take_no_ordinary_slot(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    cfg['resources']['max_local_runs'] = cfg['concurrency'] = 2
    urgent = [make_pool_card('head', cfg, conn, tmp_path, f'owner urgent {i}', URGENT_OWNER) for i in range(2)]
    d.tick()
    background = [mk(conn, f'background {i}') for i in range(3)]
    d.tick()
    assert list(launched(conn)) == [*urgent, *background[:2]]
    assert d.active_runs('task') == 2 and d.urgent_cards() == set(urgent)


def test_watchdog_script_overflows_a_full_script_pool_and_background_script_waits(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    scripts = [make_pool_card('script', cfg, conn, tmp_path, f'script {i}', {'source': 'system:test'})
               for i in range(2)]
    d.tick()
    assert list(launched(conn)) == scripts
    held = list(launched(conn).values())
    background = make_pool_card('script', cfg, conn, tmp_path, 'background script', {'source': 'system:test'})
    watchdog = make_pool_card('script', cfg, conn, tmp_path, 'watchdog script', WATCHDOG)
    d.tick()
    assert list(launched(conn))[2:] == [watchdog]
    assert claim(conn, watchdog)['bypass'] == 'cap'
    assert api.card_row(conn, background)['state'] == 'queued'
    assert_still_active(conn, held)


def test_urgent_launch_with_room_has_no_bypass(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    d.tick()
    assert claim(conn, urgent)['admission_class'] == 'urgent' and claim(conn, urgent)['bypass'] is None


@pytest.mark.parametrize('pool, kw', [('head', URGENT_OWNER), ('script', WATCHDOG)])
def test_host_psi_holds_background_while_urgent_passes_with_pressure_bypass(env, tmp_path, monkeypatch, pool, kw):
    cfg, conn, d = shared_pool(env, monkeypatch, some=90)
    d.clock = lambda: 1000
    background = make_pool_card(pool, cfg, conn, tmp_path, 'background', {'source': 'system:test'})
    urgent = make_pool_card(pool, cfg, conn, tmp_path, 'urgent', kw)
    d.tick()
    assert list(launched(conn)) == [urgent]
    assert claim(conn, urgent)['admission_class'] == 'urgent' and claim(conn, urgent)['bypass'] == 'pressure'
    assert api.card_row(conn, background)['state'] == 'queued'
    assert gate_state(conn)['last_urgent']['bypassed'] == 'io PSI some'


def test_cooldown_alone_lets_urgent_start(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    d.clock = lambda: 1000
    store.meta_set(conn, 'resource_gate', {'cpu': [0] * 8, 'blocked_until': 1030})
    background = mk(conn, 'background')
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    d.tick()
    assert list(launched(conn)) == [urgent]
    assert claim(conn, urgent)['bypass'] == 'pressure'
    assert gate_state(conn)['last_urgent']['bypassed'] == 'cooldown'
    assert api.card_row(conn, background)['state'] == 'queued'


def unavailable(**_):
    raise OSError('pressure file missing')


@pytest.mark.parametrize('floor', ['ram', 'disk', 'sample'])
def test_hard_floors_hold_urgent_under_pressure(env, tmp_path, monkeypatch, floor):
    cfg, conn, d = shared_pool(env, monkeypatch, some=90,
                               **{'ram': {'available_mb': 767}, 'disk': {'free_mb': 5119}, 'sample': {}}[floor])
    if floor == 'sample':
        monkeypatch.setattr(resources, 'sample', unavailable)
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    d.tick()
    assert not launched(conn)
    assert api.card_row(conn, urgent)['state'] == 'queued'


def test_memory_below_urgent_floor_refuses_with_the_gate_open(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch, available_mb=2000)
    cfg['resources']['reserve_mb'] = 512
    cfg['admission']['urgent_floor_mb'] = 2048
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    background = mk(conn, 'background')
    d.tick()
    assert list(launched(conn)) == [background]
    assert api.card_row(conn, urgent)['state'] == 'queued'


def test_urgent_floor_budgets_starting_runs(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch)
    background = mk(conn, 'background')
    d.tick()
    assert list(launched(conn)) == [background]
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    # One starting run at per_run_mb 768 against urgent_floor_mb 768.
    close_gate(monkeypatch, available_mb=1535)
    d.tick()
    assert api.card_row(conn, urgent)['state'] == 'queued'
    close_gate(monkeypatch, available_mb=1536)
    d.tick()
    assert claim(conn, urgent)['bypass'] == 'pressure'


def test_detached_gate_of_an_urgent_card_never_takes_the_pressure_bypass(env, tmp_path, monkeypatch):
    cfg, conn, d = shared_pool(env, monkeypatch, some=90)
    monkeypatch.setattr(dispatcher.Dispatcher, 'schedule_gate', REAL_SCHEDULE_GATE)
    gates, real_popen = [], subprocess.Popen
    def popen(argv, **kw):
        if 'hive.gate' in argv:
            return gates.append(argv)
        return real_popen(argv, **kw)
    monkeypatch.setattr(dispatcher.subprocess, 'Popen', popen)
    watchdog = make_pool_card('head', cfg, conn, tmp_path, 'watchdog', WATCHDOG)
    conn.execute("UPDATE cards SET state='check' WHERE id=?", (watchdog,))
    urgent = make_pool_card('head', cfg, conn, tmp_path, 'owner urgent', URGENT_OWNER)
    d.tick()
    assert list(launched(conn)) == [urgent] and claim(conn, urgent)['bypass'] == 'pressure'
    assert not gates and conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0] == 0
    assert api.card_row(conn, watchdog)['state'] == 'check'
