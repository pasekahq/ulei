"""Cancellation tests: durable state, process exit, dependencies and inbox dedupe."""
import json
import signal
import subprocess
import sys
import time

import pytest
from test_hive import env, mk, kinds  # noqa: F401
from hive import api, store


@pytest.mark.parametrize('state,waiting', [('created',None),('queued',None),('running',None),
                                         ('check',None),('review',None),('blocked','dependency')])
def test_cancel_idempotent(env, state, waiting):
    _, conn, d = env
    cid = mk(conn, '[ok] cancel')
    conn.execute('UPDATE cards SET state=?, wait_reason=?, recheck_due=1 WHERE id=?', (state, waiting, cid))
    assert d.cancel(cid, 'owner changed plans', 'owner')
    assert not d.cancel(cid, 'different retry reason')
    c = api.get(conn, cid)
    assert c['state'] == 'cancelled' and store.is_final(c)
    assert c['finished_at'] and c['wait_reason'] is None and c['recheck_due'] is None
    assert c['result'] == 'owner changed plans'
    assert kinds(conn, cid).count('cancelled') == 1
    row = conn.execute("SELECT actor,data FROM events WHERE card_id=? AND kind='cancelled'", (cid,)).fetchone()
    assert row['actor'] == 'owner' and json.loads(row['data'])['reason'] == 'owner changed plans'
    eid = f'hive:{cid}:cancelled'
    assert json.loads(d.adapter().inbox.packets[eid]['payload'])['state'] == 'cancelled'
    d.tick()
    d.tick()
    assert api.get(conn, cid)['state'] == 'cancelled'
    assert not api.get(conn, cid)['runs']
    assert conn.execute('SELECT count(*) FROM outbox WHERE event_id=?', (eid,)).fetchone()[0] == 1
    assert kinds(conn, cid).count('secretary_event') == 1


@pytest.mark.parametrize('actor', ['owner', 'secretary'])
def test_cancel_failed_blocked_with_reason_and_audit(env, actor):
    _, conn, d = env
    cid = mk(conn, '[ok] failed')
    d.finish(api.card_row(conn, cid), 'blocked', 'worker failed twice')
    original_version = api.get(conn, cid)['version']
    for reason in ('', '   '):
        with pytest.raises(api.CardError, match='reason is required'):
            d.cancel(cid, reason, actor)
    assert api.get(conn, cid)['version'] == original_version
    assert d.cancel(cid, 'устарело, ревизия 09.11', actor)
    assert not d.cancel(cid, 'retry reason', actor)
    c = api.get(conn, cid)
    assert c['state'] == 'cancelled' and c['block_reason'] is None
    assert c['result'] == 'устарело, ревизия 09.11'
    events = conn.execute("SELECT actor,data FROM events WHERE card_id=? AND kind='cancelled'", (cid,)).fetchall()
    assert len(events) == 1 and events[0]['actor'] == actor
    data = json.loads(events[0]['data'])
    assert data['reason'] == c['result']
    assert data['previous_state'] == 'blocked'
    assert data['previous_block_reason'] == 'worker failed twice'
    assert 'blocked' in kinds(conn, cid)
    d.tick()
    assert api.get(conn, cid)['runs'] == []
    assert cid not in {r['id'] for r in api.listing(conn)}
    assert cid in {r['id'] for r in api.listing(conn, ['cancelled'])}


def test_cancel_final_done_and_migrate(env):
    _, conn, d = env
    done = mk(conn, '[ok] done')
    conn.execute("UPDATE cards SET state='done' WHERE id=?", (done,))
    for migrate in (False, True):
        with pytest.raises(api.CardError, match='already final'):
            d.cancel(done, 'obsolete', migrate=migrate)
    cid = mk(conn, '[ok] legacy failed')
    conn.execute("UPDATE cards SET state='blocked' WHERE id=?", (cid,))
    assert d.cancel(cid, 'legacy owner cancellation', 'owner', migrate=True)
    with pytest.raises(api.CardError, match='unknown card'):
        d.cancel('h_missing')


def test_cancel_failed_blocked_cli(env, monkeypatch, capsys):
    cfg, conn, d = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    cid = mk(conn, '[ok] failed cli')
    d.finish(api.card_row(conn, cid), 'blocked', 'gate failed')
    assert cli.main(['cancel', cid]) == 2
    assert 'reason is required' in capsys.readouterr().err
    assert cli.main(['cancel', cid, '--reason', 'obsolete', '--by', 'secretary']) == 0
    capsys.readouterr()
    assert cli.main(['list', '--json']) == 0
    assert cid not in {r['id'] for r in json.loads(capsys.readouterr().out)}
    assert cli.main(['list', '--state', 'cancelled', '--json']) == 0
    assert cid in {r['id'] for r in json.loads(capsys.readouterr().out)}


def test_cancel_dependencies_and_epic(env):
    _, conn, d = env
    blocker = mk(conn, '[ok] blocker')
    dependent = mk(conn, '[ok] dependent', blocked_by=[blocker])
    epic, _ = api.create(conn, title='cancel child epic', goal='g', acceptance='true', step=1,
                         source='system:test', created_by='secretary', kind='epic')
    child = mk(conn, '[ok] child', parent=epic)
    d.cancel(blocker)
    d.cancel(child)
    assert d.unblock_dependencies() == 1
    assert api.get(conn, dependent)['state'] == 'created'
    assert d.check_epics() == 1
    assert api.get(conn, epic)['state'] == 'check'
    late = mk(conn, '[ok] late dependency', blocked_by=[blocker])
    assert api.get(conn, late)['state'] == 'created'


def test_cancel_series(env):
    _, conn, d = env
    cid = mk(conn, '[ok] series', kind='recurring', every=60, max_runs=3)
    assert d.cancel(cid, 'cancel series')
    d.tick()
    c = api.get(conn, cid)
    assert c['state'] == 'cancelled' and c['schedule']['stopped']
    assert c['instances'] == []


@pytest.mark.parametrize('overlap', ['skip', 'queue'])
def test_cancel_failed_instance_keeps_series_running(env, overlap):
    _, conn, d = env
    series = mk(conn, '[ok] recurring', kind='recurring', every=60,
                max_runs=3, overlap=overlap, not_before=d.clock() - 1)
    now = d.clock()
    assert d.advance_series(now) == 1
    first = api.get(conn, series)['instances'][0]['id']
    d.finish(api.card_row(conn, first), 'blocked', 'worker failed twice', wait_reason=None)
    assert d.cancel(first, 'obsolete')
    d.advance_series(now + 60)
    s = api.get(conn, series)
    assert s['state'] == 'blocked' and s['schedule']['stopped'] is False
    assert len(s['instances']) == 2
    assert s['instances'][0]['state'] == 'cancelled'
    assert s['instances'][1]['state'] == 'created'
    assert api.get(conn, first)['runs'] == []


@pytest.mark.parametrize('role,state', [('worker', 'running'), ('reviewer', 'review'),
                                         ('worker', 'blocked'), ('reviewer', 'blocked')])
def test_cancel_waits_for_process_exit(env, role, state):
    _, conn, d = env
    cid = mk(conn, '[ok] live cancellation')
    proc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)
    try:
        conn.execute("UPDATE cards SET state=?, round=1 WHERE id=?", (state, cid))
        conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,pid,started_at)
                        VALUES(?,1,?,'codex','fake','low',1,'test','running',?,?)""", (cid, role, proc.pid, time.time()))
        assert d.cancel(cid, 'obsolete')
        assert proc.wait(timeout=1) == -signal.SIGTERM
        assert conn.execute('SELECT status FROM runs WHERE card_id=?', (cid,)).fetchone()[0] == 'interrupted'
        d.advance()
        assert api.get(conn, cid)['state'] == 'cancelled'
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()


def test_cancel_crash_recovery(env, monkeypatch):
    _, conn, d = env
    cid = mk(conn, '[ok] durable cancel')
    real_stop = d.stop_cancelled_runs
    def lost_stop(*args, **kwargs):
        raise RuntimeError('simulated crash')
    monkeypatch.setattr(d, 'stop_cancelled_runs', lost_stop)
    with pytest.raises(RuntimeError):
        d.cancel(cid, 'keep this reason')
    assert api.get(conn, cid)['state'] == 'cancelled'
    monkeypatch.setattr(d, 'stop_cancelled_runs', real_stop)
    assert not d.cancel(cid, 'retry')
    assert d.adapter().inbox.get(f'hive:{cid}:cancelled')
    assert kinds(conn, cid).count('cancelled') == 1


def test_cancel_cli_and_legacy_candidates(env, monkeypatch, capsys):
    cfg, conn, d = env
    from scripts import hive as cli
    from scripts.hive_migrate_cancellations import candidates
    cid = mk(conn, '[ok] cli cancellation')
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    assert cli.main(['cancel', cid, '--reason', 'test card no longer needed', '--by', 'owner']) == 0
    assert cli.main(['cancel', cid]) == 0
    assert 'already cancelled' in capsys.readouterr().out
    assert api.get(conn, cid)['state'] == 'cancelled'
    legacy = mk(conn, '[ok] legacy')
    conn.execute("UPDATE cards SET state='blocked',block_reason=? WHERE id=?",
                 (json.dumps({'kind':'cancelled', 'by':'owner', 'note':'owner stopped'}), legacy))
    assert candidates(conn, cfg['owner_actor']) == {legacy: ('owner stopped', 'owner')}
    d.cancel(legacy, 'owner stopped', 'owner', migrate=True)
    assert candidates(conn, cfg['owner_actor']) == {}


def test_cancel_invalidates_unclaimed_runner(env):
    _, conn, d = env
    cid = mk(conn, '[ok] unclaimed')
    conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                    VALUES(?,1,'worker','codex','fake','low',1,'test','starting',?)""", (cid,time.time()))
    assert d.cancel(cid)
    assert conn.execute('SELECT status FROM runs WHERE card_id=?', (cid,)).fetchone()[0] == 'interrupted'
    assert conn.execute("UPDATE runs SET status='running' WHERE card_id=? AND status='starting'", (cid,)).rowcount == 0


def test_cancel_uses_pid_of_runner_claimed_after_scan(env, monkeypatch):
    from contextlib import contextmanager
    _, conn, d = env
    cid = mk(conn, '[ok] claim race')
    conn.execute("UPDATE cards SET state='cancelled' WHERE id=?", (cid,))
    conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                    VALUES(?,1,'worker','codex','fake','low',1,'test','starting',?)""", (cid,time.time()))
    proc = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(120)'], start_new_session=True)
    original_tx, claimed = store.tx, []
    @contextmanager
    def claim_before_stop(db):
        if not claimed:
            db.execute("UPDATE runs SET status='running',pid=? WHERE card_id=?", (proc.pid,cid))
            claimed.append(True)
        with original_tx(db):
            yield db
    monkeypatch.setattr(store, 'tx', claim_before_stop)
    try:
        d.stop_cancelled_runs(cid, wait=True)
        assert proc.wait(timeout=1) == -signal.SIGTERM
    finally:
        if proc.poll() is None:
            proc.kill()
        proc.wait()
