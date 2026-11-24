"""Audit 3/4/5 regression checks: real detached gates and fake heads only."""
import fcntl
import hashlib
import os
import subprocess
import sys
import json
import time
from pathlib import Path

import pytest
from test_hive import env, mk, run_until, kinds, REAL_SCHEDULE_GATE, ROOT
from hive import api, store, dispatcher


def row(conn, cid):
    return conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()


def create(conn, **kw):
    opts = dict(title='audit', goal='verify', acceptance='true', step=1, source='system:test', created_by='secretary')
    opts.update(kw)
    return api.create(conn, **opts)[0]


@pytest.mark.parametrize('command', ['test -s REPORT.md', 'test -s "./REPORT.md";', '[ -s REPORT.md ]'])
def test_empty_gate_requires_review(env, command):
    _, conn, _ = env
    with pytest.raises(api.CardError, match='requires review'):
        create(conn, acceptance=command)
    assert row(conn, create(conn, acceptance=command, review=True))['review'] == 1


@pytest.mark.parametrize('timeout', [0, -1, True, float('inf'), float('nan')])
def test_invalid_card_timeout(env, timeout):
    with pytest.raises(api.CardError, match='gate_timeout'):
        create(env[1], gate_timeout=timeout)


def test_slow_gate_does_not_hold_tick_or_lock_and_cancel_wins(env):
    cfg, conn, d = env
    d.schedule_gate = REAL_SCHEDULE_GATE.__get__(d)
    cid = create(conn, acceptance='touch entered; while [ ! -e released ]; do sleep .05; done; touch finished', gate_timeout=30)
    conn.execute("UPDATE cards SET state='check', round=1 WHERE id=?", (cid,))
    d.tick()
    root = Path(cfg['workspaces']) / cid
    end = time.monotonic() + 10
    while not (root / 'entered').exists() and time.monotonic() < end:
        time.sleep(.05)
    assert (root / 'entered').exists()
    assert not (root / 'finished').exists()
    with Path(cfg['db']).with_name('dispatcher.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    assert conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0] == 1
    d.tick()
    assert conn.execute('SELECT count(*) FROM gate_jobs').fetchone()[0] == 1
    d.cancel(cid, 'test cancellation', actor='owner')
    (root / 'released').touch()
    end = time.monotonic() + 10
    while conn.execute('SELECT 1 FROM gate_jobs').fetchone() and time.monotonic() < end:
        time.sleep(.05)
    d.tick()
    assert not conn.execute('SELECT 1 FROM gate_jobs').fetchone()
    assert (root / 'finished').exists()
    assert row(conn, cid)['state'] == 'cancelled'
    assert not conn.execute('SELECT 1 FROM gates WHERE card_id=?', (cid,)).fetchone()


def test_card_timeout_and_identical_gate_stop_without_round_cap(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1000000
    cid = create(conn, acceptance='sleep 20', gate_timeout=.05, deliver='local')
    for rnd in (1, 2):
        conn.execute("UPDATE cards SET state='check', round=?, version=version+1 WHERE id=?", (rnd, cid))
        d.gate(row(conn, cid))
    assert row(conn, cid)['state'] == 'blocked'
    assert 'loop_stopped' in kinds(conn, cid)
    assert conn.execute('SELECT output FROM gates WHERE card_id=? LIMIT 1', (cid,)).fetchone()[0] == 'gate timeout'
    d.publish()
    assert d.inbox().get(f'hive:{cid}:blocked') is not None


def report_run(conn, cid, rnd, report):
    rid = conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,
                        report) VALUES(?,?,'worker','codex','fake','low',1,'','exited',?,?)""",
                       (cid, rnd, time.time(), report)).lastrowid
    conn.execute('UPDATE runs SET report_sha256=?, report_tail=? WHERE id=?',
                 (hashlib.sha256(report.encode()).hexdigest(), report[-2500:], rid))
    store.event(conn, cid, 'run_report', {'run': rid, 'file': 'REPORT.md', 'source': 'file'})
    return conn.execute('SELECT * FROM runs WHERE id=?', (rid,)).fetchone()


def test_report_repeat_and_payload_tail(env):
    _, conn, d = env
    cid = create(conn)
    report = 'RESULT: needs-owner owner: подтвердить доступ.\n' + 'x' * 4000 + '\nTAIL BLOCKER'
    report_run(conn, cid, 1, report)
    r = report_run(conn, cid, 2, report)
    conn.execute("UPDATE cards SET state='running', round=2 WHERE id=?", (cid,))
    with store.tx(conn):
        assert d.repeated_report(row(conn, cid), r)
    payload = d.result_payload(cid, 'blocked', 'loop')
    assert next(iter(payload)) == 'result_line'
    assert payload['report_excerpt'].startswith('RESULT: needs-owner')
    assert payload['report_excerpt'].endswith('TAIL BLOCKER')


def test_owner_priority_and_fifth_slot(env, monkeypatch):
    cfg, conn, d = env
    cfg['concurrency'] = 5
    cfg['admission']['urgent_reserve'] = 1
    monkeypatch.setattr(d, 'spawn', lambda *args: None)
    backgrounds = [create(conn, title=f'background {i}', priority=999) for i in range(6)]
    d.tick()
    assert d.active_runs('task') == 4
    owner = create(conn, created_by='owner', source='msg:owner:123')
    assert row(conn, owner)['priority'] == 100
    d.tick()
    assert row(conn, owner)['state'] == 'running'
    assert d.active_runs('task') == 5
    assert sum(row(conn, c)['state'] == 'queued' for c in backgrounds) == 2


def test_different_verbose_gate_outputs_do_not_stop(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1000000
    cid = create(conn, acceptance="cat prefix; head -c 5000 /dev/zero; exit 1")
    root = Path(cfg['workspaces']) / cid
    root.mkdir(parents=True)
    for rnd in (1, 2):
        (root / 'prefix').write_text(str(rnd))
        conn.execute("UPDATE cards SET state='check', round=?, workdir=?, version=version+1 WHERE id=?", (rnd, str(root), cid))
        d.gate(row(conn, cid))
    assert row(conn, cid)['state'] == 'queued'
    assert 'loop_stopped' not in kinds(conn, cid)


@pytest.mark.parametrize('identical', [False, True])
def test_full_report_identity_and_tail_through_runner(env, identical):
    """Exercise actual runner capture, not pre-populated digest/tail fields."""
    from hive.runner import REPORT_LIMIT, REPORT_TAIL_LIMIT
    cfg, conn, d = env
    cid = create(conn)
    workspace = Path(cfg['workspaces']) / cid
    workspace.mkdir(parents=True)
    first = 'RESULT: partial owner: проверить блокер в конце.\n' + 'я' * REPORT_LIMIT + '\nBLOCKER ONE'
    second = first if identical else first.removesuffix('ONE') + 'TWO'
    for rnd, report in enumerate((first, second), 1):
        run_dir = workspace / f'run{rnd}'
        run_dir.mkdir()
        rid = conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,
                              status,started_at,run_dir)
                              VALUES(?,?,'worker','codex','fake','low',1,'','starting',?,?)""",
                           (cid, rnd, time.time(), str(run_dir))).lastrowid
        job = dict(cwd=str(workspace), report_file='REPORT.md', vendor='codex', role='worker',
                   env=dict(os.environ), prompt='', timeout=10,
                   argv=[sys.executable, '-c', "from pathlib import Path; Path('REPORT.md').write_text("
                         + repr(report) + ", encoding='utf-8')"])
        (run_dir / 'job.json').write_text(json.dumps(job))
        subprocess.run([sys.executable, '-m', 'hive.runner', '--db', cfg['db'], '--run-id', str(rid)],
                       env=dict(os.environ, PYTHONPATH=str(ROOT)), check=True, capture_output=True)
        r = conn.execute('SELECT * FROM runs WHERE id=?', (rid,)).fetchone()
        assert r['report'] == report[:REPORT_LIMIT]
        assert r['report_sha256'] == hashlib.sha256(report.encode()).hexdigest()
        assert r['report_tail'] == report[-REPORT_TAIL_LIMIT:]
        assert api.fresh_report(conn, rid) is not None
    conn.execute("UPDATE cards SET state='running', round=2 WHERE id=?", (cid,))
    with store.tx(conn):
        assert d.repeated_report(row(conn, cid), r) is identical
    assert row(conn, cid)['state'] == ('blocked' if identical else 'running')
    assert ('loop_stopped' in kinds(conn, cid)) is identical
    payload = d.result_payload(cid, 'blocked' if identical else 'running', 'test')
    assert payload['result_line'] == second.splitlines()[0]
    assert payload['report_excerpt'].startswith(payload['result_line'])
    assert payload['report_excerpt'].endswith(second[-REPORT_TAIL_LIMIT:])


def test_legacy_report_prefix_does_not_prove_identity(env):
    _, conn, d = env
    cid = create(conn)
    report_run(conn, cid, 1, 'same prefix')
    r = report_run(conn, cid, 2, 'same prefix')
    conn.execute('UPDATE runs SET report_sha256=NULL WHERE card_id=?', (cid,))
    r = conn.execute('SELECT * FROM runs WHERE id=?', (r['id'],)).fetchone()
    assert not d.repeated_report(row(conn, cid), r)


@pytest.mark.parametrize('policy', ['recheck', 'series', 'conditional'])
def test_scheduled_wait_is_not_a_loop(env, policy):
    cfg, conn, d = env
    cfg['max_rounds'] = 1000000
    opts = {'recheck_every': 21600, 'recheck_max': 20} if policy == 'recheck' else {}
    cid = create(conn, acceptance='false', **opts)
    if policy == 'series':
        conn.execute("UPDATE cards SET series_period=0, card_class='series_run' WHERE id=?", (cid,))
    if policy == 'conditional':
        conn.execute("UPDATE cards SET card_class='series_run', source='cron:test:conditional' WHERE id=?", (cid,))
        assert row(conn, cid)['series_period'] is None
    for rnd in (1, 2, 3):
        r = report_run(conn, cid, rnd, 'RESULT: partial waiting for external release')
        conn.execute("UPDATE cards SET state='running', round=? WHERE id=?", (rnd, cid))
        with store.tx(conn):
            assert not d.repeated_report(row(conn, cid), r)
        conn.execute("UPDATE cards SET state='check', version=version+1 WHERE id=?", (cid,))
        d.gate(row(conn, cid))
        assert row(conn, cid)['state'] == 'queued'
    assert 'loop_stopped' not in kinds(conn, cid)
    assert conn.execute('SELECT count(*) FROM gates WHERE card_id=?', (cid,)).fetchone()[0] == 3
