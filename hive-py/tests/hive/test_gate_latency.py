"""An owner-visible acceptance check starts when the head exits, not when host pressure clears.

Admitted as a non-urgent launch, the check of an owner card waits out the resource gate's PSI cooldown, and its
result then waits for the next tick to be published.
"""
import subprocess
import sys
from pathlib import Path

from test_hive import env, mk, run_until, REAL_SCHEDULE_GATE  # noqa: F401
from test_resources import healthy
from hive import api, dispatcher, gate, heads, resources, store


def closed_gate(cfg, conn, monkeypatch, **sample):
    """Soft PSI pressure plus a durable cooldown, RAM/disk healthy unless overridden."""
    cfg['resources'] = dict(enabled=True, max_local_runs=2, io_some_pct=70, io_full_pct=50, iowait_pct=45,
                            disk=str(Path(cfg['db']).parent), psi_cgroups=[])
    monkeypatch.setattr(resources, 'sample', lambda **_: {**healthy(), 'some': 90, **sample})
    store.meta_set(conn, 'resource_gate', {'cpu': [0] * 8})


def detached(monkeypatch):
    monkeypatch.setattr(dispatcher.Dispatcher, 'schedule_gate', REAL_SCHEDULE_GATE)
    calls, real_popen = [], subprocess.Popen
    def popen(argv, **kw):
        if 'hive.gate' in argv:
            return calls.append(argv)
        return real_popen(argv, **kw)
    monkeypatch.setattr(dispatcher.subprocess, 'Popen', popen)
    return calls


def in_check(conn, title, **kw):
    cid = mk(conn, title, **kw)
    conn.execute("UPDATE cards SET state='check', version=version+1 WHERE id=?", (cid,))
    return cid


def gated(conn):
    return {r[0] for r in conn.execute('SELECT card_id FROM gate_jobs')}


def test_owner_check_starts_under_soft_pressure_background_waits(env, monkeypatch):
    cfg, conn, d = env
    closed_gate(cfg, conn, monkeypatch)
    calls = detached(monkeypatch)
    owner = in_check(conn, 'owner', card_class='owner_task')
    visible = in_check(conn, 'visible series', owner_visible=True)
    background = in_check(conn, 'background')
    d.tick()
    assert gated(conn) == {owner, visible}
    assert len(calls) == 2 and all('hive.gate' in argv for argv in calls)
    assert api.card_row(conn, background)['state'] == 'check'


def test_owner_check_ignores_a_full_pool(env, monkeypatch):
    cfg, conn, d = env
    closed_gate(cfg, conn, monkeypatch, some=0)
    detached(monkeypatch)
    for i in range(2):  # max_local_runs=2 already taken by other detached checks
        conn.execute('INSERT INTO gate_jobs VALUES(?,?,?,?)', (f'h_busy{i}', 1, 'gate', d.clock()))
    owner = in_check(conn, 'owner', card_class='owner_task')
    background = in_check(conn, 'background')
    d.tick()
    assert owner in gated(conn) and background not in gated(conn)


def test_owner_check_still_honours_ram_and_disk_floors(env, monkeypatch):
    cfg, conn, d = env
    for floor in ({'available_mb': 100}, {'free_mb': 0}):
        conn.execute('DELETE FROM gate_jobs')
        closed_gate(cfg, conn, monkeypatch, **floor)
        cfg['resources']['min_free_mb'] = 1024
        detached(monkeypatch)
        owner = in_check(conn, f'owner {floor}', card_class='owner_task')
        dispatcher.Dispatcher(cfg, conn).tick()
        assert owner not in gated(conn)


def test_owner_checks_run_before_slow_bookkeeping(env, monkeypatch):
    cfg, conn, d = env
    detached(monkeypatch)
    owner = in_check(conn, 'owner', card_class='owner_task')
    seen = []
    real = d.advance_series
    def slow_series(now):
        seen.append(owner in gated(conn))
        return real(now)
    monkeypatch.setattr(d, 'advance_series', slow_series)
    d.tick()
    assert seen and seen[0] is True


def test_gate_worker_publishes_the_result_without_waiting_for_a_tick(env, monkeypatch):
    cfg, conn, d = env
    calls = detached(monkeypatch)
    owner = mk(conn, '[ok] owner', card_class='owner_task')
    assert run_until(d, conn, owner, states=('check',)) == 'check'
    for _ in range(50):
        if calls:
            break
        d.tick()
    argv = calls[0]
    monkeypatch.setattr(sys, 'argv', argv[argv.index('hive.gate'):])
    gate.main()  # the detached worker, without any further dispatcher tick
    assert api.card_row(conn, owner)['state'] == 'done'
    kinds = [r[0] for r in conn.execute('SELECT kind FROM events WHERE card_id=? ORDER BY id', (owner,))]
    assert kinds[-4:] == ['gate', 'judge', 'done', 'secretary_event']
    assert not gated(conn)
    assert conn.execute('SELECT count(*) FROM outbox WHERE card_id=? AND published_at IS NULL', (owner,)).fetchone()[0] == 0


def test_racing_publishers_emit_one_secretary_event(env, monkeypatch):
    cfg, conn, d = env
    owner = mk(conn, 'owner', card_class='owner_task')
    d.finish(api.card_row(conn, owner), 'done', 'gate passed')
    event_id = conn.execute('SELECT event_id FROM outbox WHERE card_id=?', (owner,)).fetchone()[0]
    real = d.publish_inbox
    def other_publisher_wins(o):
        real(o)
        conn.execute('UPDATE outbox SET published_at=1 WHERE event_id=?', (o['event_id'],))
    monkeypatch.setattr(d, 'publish_inbox', other_publisher_wins)
    assert d.publish(event_id) == 0
    assert conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='secretary_event'", (owner,)).fetchone()[0] == 0
