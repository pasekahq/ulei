"""Read-only snapshots: one card from the inside, the hive as a whole, the filtered list and the workspace's files."""
import json
import os
from pathlib import Path

import pytest
from test_hive import env, mk, mk_series  # noqa: F401
from test_liveness import manager, start_unit, unit_dir
from fake_delivery import FakeAdapter
from hive import api, inspect, store

NOW = 2_000_000_000.0
HZ = os.sysconf('SC_CLK_TCK')
UPTIME = 100_000.0


class FakeGh:
    """The `gh` lookups of a probe: answers by pull request or workflow run number, recording each call."""

    def __init__(self, pulls=None, runs=None):
        self.pulls, self.runs, self.calls = pulls or {}, runs or {}, []


def probe(tmp_path, gh=None):
    p = inspect.Probe(cgroup_root=tmp_path / 'cg', proc_root=tmp_path / 'proc', gh=None)
    if gh is not None:
        p.pull = lambda n, repo, cwd: (gh.calls.append(('pr', n, repo)), gh.pulls.get(n, {'error': 'none'}))[1]
        p.workflow_run = lambda n, repo, cwd: (gh.calls.append(('run', n, repo)), gh.runs.get(n, {'error': 'none'}))[1]
    return p


def proc(tmp_path, pid, ppid, argv, age):
    """A fake /proc entry: its command line, a stat whose start time is `age` seconds before the fake uptime."""
    root = tmp_path / 'proc'
    root.mkdir(exist_ok=True)
    (root / 'uptime').write_text(f'{UPTIME} 0\n')
    d = root / str(pid)
    d.mkdir()
    (d / 'cmdline').write_bytes(b'\0'.join(a.encode() for a in argv) + b'\0')
    start = int((UPTIME - age) * HZ)
    fields = ['S', str(ppid)] + ['0'] * 17 + [str(start)] + ['0'] * 5
    (d / 'stat').write_text(f'{pid} (x) ' + ' '.join(fields) + '\n')


def unit(tmp_path, name, pids):
    start_unit(tmp_path / 'cg', name)
    (unit_dir(tmp_path / 'cg', name) / 'cgroup.procs').write_text(''.join(f'{p}\n' for p in pids))


def running(conn, cid, unit_name, pid, role='worker', rnd=1):
    conn.execute("UPDATE cards SET state='running', round=? WHERE id=?", (rnd, cid))
    return conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,pid,unit,started_at,
                           heartbeat_at) VALUES(?,?,?,'fake','m1','high',1,'',?,?,?,?,?)""",
                        (cid, rnd, role, 'running', pid, unit_name, NOW - 600, NOW - 5)).lastrowid


def finished(conn, cid, rnd, role, report, verdict=None, outcome='ok'):
    return conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,ended_at,
                           duration_s,exit_code,outcome,verdict,report,report_file)
                           VALUES(?,?,?,'fake','m1','medium',1,'','exited',?,?,60,0,?,?,?,'REPORT.md')""",
                        (cid, rnd, role, NOW - 5000 + rnd * 100, NOW - 4900 + rnd * 100, outcome, verdict, report)).lastrowid


def test_pulse_names_the_command_a_head_waits_on_its_unit_and_pull_request(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'waits on CI')
    manager(tmp_path / 'cg')
    # runner 500 → head 501 → command 502 (a wait loop on another unit and a PR) → sleep 503
    loop = "until ! systemctl --user is-active -q swarm-poll; do gh pr checks 42 -R org/repo --watch 2>/dev/null; sleep 30; done"
    proc(tmp_path, 500, 1, ['python', '-m', 'hive.runner'], 600)
    proc(tmp_path, 501, 500, ['vendor-cli', '--print'], 590)
    proc(tmp_path, 502, 501, ['/bin/bash', '-c', loop], 400)
    proc(tmp_path, 503, 502, ['sleep', '30'], 3)
    unit(tmp_path, 'hive-run-9', [500, 501, 502, 503])
    proc(tmp_path, 600, 1, ['bash', '-c', 'make all'], 900)
    proc(tmp_path, 601, 600, ['cc', 'x.c'], 2)
    unit(tmp_path, 'swarm-poll', [600, 601])
    running(conn, cid, 'hive-run-9', 500)
    gh = FakeGh(pulls={42: {'number': 42, 'url': 'u', 'state': 'OPEN', 'mergeStateStatus': 'BLOCKED',
                            'statusCheckRollup': [{'name': 'a', 'status': 'COMPLETED', 'conclusion': 'SUCCESS'},
                                                  {'name': 'b', 'status': 'IN_PROGRESS', 'conclusion': ''},
                                                  {'name': 'c', 'status': 'COMPLETED', 'conclusion': 'FAILURE'}]}})
    snap = inspect.card(conn, cfg, cid, NOW, probe(tmp_path, gh), FakeAdapter())
    [head] = snap['heads']['heads']
    assert head['unit'] == 'hive-run-9' and head['populated'] and head['running_s'] == 600
    [command] = head['commands']
    assert command['pid'] == 502 and command['age_s'] == pytest.approx(400, abs=1)
    assert command['now']['cmdline'] == 'sleep 30'
    [linked] = head['linked_units']
    assert linked['unit'] == 'swarm-poll' and linked['commands'][0]['now']['cmdline'] == 'cc x.c'
    [pr] = head['pull_requests']
    assert (pr['pr'], pr['repo'], pr['state']) == (42, 'org/repo', 'OPEN')
    assert pr['checks'] == {'SUCCESS': 1, 'PENDING': 1, 'FAILURE': 1}
    assert pr['failed'] == ['c'] and pr['pending'] == ['b']
    assert gh.calls == [('pr', 42, 'org/repo')]
    w = head['waiting_on']
    assert w['unit'] == 'swarm-poll' and w['for_s'] == pytest.approx(900, abs=1) and w['pull_requests'] == [42]
    assert snap['queue']['column'] == 'in_progress'


def test_pulse_reads_workflow_runs_and_strips_a_vendor_shell_preamble(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'watches a run')
    manager(tmp_path / 'cg')
    proc(tmp_path, 500, 1, ['runner'], 60)
    proc(tmp_path, 501, 500, ['vendor-cli'], 59)
    wrapped = "source /tmp/snap.sh 2>/dev/null || true && shopt -u extglob && eval 'gh run watch 77 --repo org/r'"
    proc(tmp_path, 502, 501, ['/bin/bash', '-c', wrapped], 50)
    unit(tmp_path, 'hive-run-3', [500, 501, 502])
    running(conn, cid, 'hive-run-3', 500)
    gh = FakeGh(runs={77: {'name': 'CI', 'status': 'completed', 'conclusion': 'success'}})
    [head] = inspect.card(conn, cfg, cid, NOW, probe(tmp_path, gh))['heads']['heads']
    assert head['commands'][0]['cmdline'] == "gh run watch 77 --repo org/r'"
    [run] = head['workflow_runs']
    assert (run['run'], run['repo'], run['status'], run['conclusion']) == (77, 'org/r', 'completed', 'success')
    assert head['pull_requests'] == []


def test_a_redirect_is_not_a_pull_request_number(env, tmp_path):
    procs = [{'scan': 'gh pr checks 261 -R org/r --json name,bucket 2>/dev/null', 'cmdline': 'x', 'cwd': None,
              'age_s': 1}]
    assert [p[:2] for p in inspect.pulls(procs)] == [(261, 'org/r')]


def test_rounds_carry_runs_reports_gate_and_verdicts(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'reviewed', review=True)
    conn.execute("UPDATE cards SET state='done', round=2 WHERE id=?", (cid,))
    finished(conn, cid, 1, 'worker', 'RESULT: done first try')
    finished(conn, cid, 1, 'reviewer', 'VERDICT: FAIL\nno live check', verdict='FAIL')
    finished(conn, cid, 2, 'worker', 'RESULT: done with the live check')
    finished(conn, cid, 2, 'reviewer', 'VERDICT: PASS', verdict='PASS')
    for rnd, passed in ((1, 1), (2, 1)):
        conn.execute("INSERT INTO gates(card_id,round,command,exit_code,passed,output,at) VALUES(?,?,?,?,?,?,?)",
                     (cid, rnd, 'test -s out.txt', 0, passed, '', NOW - 4000 + rnd))
    store.event(conn, cid, 'judge', {'round': 1, 'route': 'r', 'verdict': 'rework', 'reason': 'criterion 3: no live check'})
    store.event(conn, cid, 'retry', {'round': 1, 'reason': 'judge rework', 'delay_s': 300})
    store.event(conn, cid, 'judge', {'round': 2, 'route': 'r', 'verdict': 'done', 'reason': ''})
    snap = inspect.card(conn, cfg, cid, NOW, probe(tmp_path))
    r1, r2 = snap['rounds']
    assert r1['verdict'] == {'result': 'FAIL', 'by': 'judge', 'detail': 'rework'}
    assert r1['judge']['reason'] == 'criterion 3: no live check'
    assert [e['kind'] for e in r1['events']] == ['retry']
    assert [(r['role'], r['model'], r['effort'], r['verdict']) for r in r1['runs']] == [
        ('worker', 'm1', 'medium', None), ('reviewer', 'm1', 'medium', 'FAIL')]
    assert r1['runs'][0]['report'] == 'RESULT: done first try' and r1['gate']['passed'] is True
    assert r2['verdict']['result'] == 'PASS'
    assert snap['heads']['heads'] == []


def test_links_steers_and_deliveries(env, tmp_path):
    cfg, conn, _ = env
    first = mk(conn, 'first')
    epic, _ = api.create(conn, title='epic', goal='g', acceptance='true', step=1, source='system:test',
                         created_by='secretary', kind='epic')
    second, _ = api.create(conn, title='second', goal='g', acceptance='test -s out.txt', step=1, source='system:test',
                           created_by='secretary', followup_of=first, parent=epic, card_class='followup')
    api.steer(conn, second, 'please also check the docs', 'secretary')
    adapter = FakeAdapter()
    conn.execute("INSERT INTO outbox(event_id,card_id,kind,payload,source_ts,created_at,published_at) "
                     "VALUES(?,?,?,?,?,?,?)", (f'hive:{second}:done', second, 'task_result', '{}', NOW, NOW, NOW))
    from hive.delivery import Event
    adapter.deliver(Event('task_result', f'hive:{second}:done', '{}', NOW))
    snap = inspect.card(conn, cfg, second, NOW, probe(tmp_path), adapter)
    assert snap['links']['followup_of']['id'] == first and snap['links']['parent']['id'] == epic
    assert [s['text'] for s in snap['steers']] == ['please also check the docs']
    assert snap['steers'][0]['delivery'] == 'pending'
    [d] = snap['deliveries']
    assert d['inbox'] == {'status': 'done', 'attempts': 1, 'outcome': 'success'}
    back = inspect.card(conn, cfg, first, NOW, probe(tmp_path))
    assert [f['id'] for f in back['links']['followups']] == [second]


def test_queue_position_slot_holders_and_why_it_waits(env, tmp_path):
    cfg, conn, _ = env
    cfg['concurrency'] = 2
    holders = [mk(conn, f'busy {i}') for i in range(2)]
    for i, h in enumerate(holders):
        running(conn, h, f'hive-run-{i}', None)
    a, b = mk(conn, 'first in line'), mk(conn, 'second in line')
    for c in (a, b):
        conn.execute("UPDATE cards SET state='queued' WHERE id=?", (c,))
    snap = inspect.card(conn, cfg, b, NOW, probe(tmp_path))
    q = snap['queue']
    assert (q['position'], q['queue_length'], q['ahead']) == (2, 2, [a])
    assert q['slots']['used'] == 2 and q['slots']['free'] == 0
    assert sorted(h['card_id'] for h in q['slots']['holders']) == sorted(holders)
    assert q['wait']['cause'] == 'queue'


def test_health_names_limits_tick_slots_queue_and_series(env, tmp_path):
    cfg, conn, d = env
    rule = mk_series(conn, 'every hour', every=3600, max_runs=3)
    d.tick()
    h = inspect.health(conn, cfg, NOW, FakeAdapter())
    assert h['config']['concurrency'] == cfg['concurrency']
    assert not {'head_env', 'gate_env', 'script_env', 'texts', 'db', 'inbox'} & set(h['config'])
    assert h['dispatcher']['last_tick_at'] is not None
    [series] = [s for s in h['series'] if s['id'] == rule]
    assert series['schedule']['every'] == 3600 and series['schedule']['max_runs'] == 3
    json.dumps(h)


def test_find_filters_and_searches(env, tmp_path):
    cfg, conn, _ = env
    a = mk(conn, 'Telegram reactions')
    epic, _ = api.create(conn, title='epic', goal='g', acceptance='true', step=1, source='system:test',
                         created_by='secretary', kind='epic')
    b, _ = api.create(conn, title='child', goal='touch the 50%_x reactions', acceptance='test -s out.txt', step=1,
                      source='system:test', created_by='owner', parent=epic)
    assert {r['id'] for r in inspect.find(conn, query='REACTIONS')} == {a, b}
    assert [r['id'] for r in inspect.find(conn, query='50%_x')] == [b]
    assert [r['id'] for r in inspect.find(conn, parent=epic)] == [b]
    assert [r['id'] for r in inspect.find(conn, created_by='owner')] == [b]
    assert a in [r['id'] for r in inspect.find(conn, placement='board')]
    assert a not in [r['id'] for r in inspect.find(conn, placement='hidden')]
    with pytest.raises(ValueError):
        inspect.find(conn, placement='nowhere')


def test_read_file_stays_in_the_workspace(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'files')
    work = tmp_path / 'ws'
    (work / 'ev').mkdir(parents=True)
    (work / 'REPORT.md').write_text('RESULT: done\n' + 'x' * 100)
    (work / 'ev' / 'log.txt').write_text('evidence')
    (work / '.env').write_text('SECRET=1')
    (tmp_path / 'outside.txt').write_text('no')
    (work / 'escape').symlink_to(tmp_path / 'outside.txt')
    results = Path(cfg['results_dir'])
    results.mkdir(parents=True, exist_ok=True)
    (results / 'named.md').write_text('result file')
    (results / 'other.md').write_text('another card')
    conn.execute('UPDATE cards SET workdir=?, goal=? WHERE id=?', (str(work), f'write {results}/named.md', cid))
    c = conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()
    assert inspect.read_file(cfg, c, 'REPORT.md', limit=12)['text'] == 'RESULT: done'
    assert inspect.read_file(cfg, c, 'REPORT.md', limit=12)['truncated'] is True
    assert inspect.read_file(cfg, c, 'ev/log.txt')['text'] == 'evidence'
    assert [e['path'] for e in inspect.read_file(cfg, c, 'ev')['entries']] == ['ev/log.txt']
    assert inspect.read_file(cfg, c, '@out/named.md')['text'] == 'result file'
    for bad in ('../outside.txt', '/etc/passwd', 'escape', '.env', 'ev/../REPORT.md', '@out/other.md', ''):
        with pytest.raises(inspect.FileError):
            inspect.read_file(cfg, c, bad)
    snap = inspect.card(conn, cfg, cid, NOW, probe(tmp_path))
    paths = {e['path']: e for e in snap['files']['entries']}
    assert 'REPORT.md' in paths and 'ev/log.txt' in paths and paths['.env']['readable'] is False
    assert [e['path'] for e in snap['files']['results']] == ['@out/named.md']


def test_cli_inspect_health_file_and_list_filters(env, tmp_path, capsys):
    from scripts import hive as cli
    cfg, conn, _ = env
    cid = mk(conn, 'cli card')
    assert cli.main(['inspect', cid, '--json', '--no-github']) == 0
    snap = json.loads(capsys.readouterr().out)
    assert snap['card_id'] == cid and snap['version'] == inspect.VERSION
    assert cli.main(['inspect', 'h_00000000', '--json']) == 1
    assert cli.main(['health', '--json']) == 0
    assert 'slots' in json.loads(capsys.readouterr().out)
    assert cli.main(['list', '--json', '--query', 'cli card']) == 0
    assert [r['id'] for r in json.loads(capsys.readouterr().out)] == [cid]
    assert cli.main(['file', cid, '../x']) == 2
