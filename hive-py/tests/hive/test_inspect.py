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
    assert w['unit'] == 'hive-run-9' and w['for_s'] == pytest.approx(400, abs=1) and w['now'] == 'sleep 30'
    assert w['linked_units'] == ['swarm-poll'] and w['pull_requests'] == [42]
    assert snap['queue']['column'] == 'in_progress'


def test_pulse_reads_workflow_runs_and_strips_a_vendor_shell_preamble(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'watches a run')
    manager(tmp_path / 'cg')
    proc(tmp_path, 500, 1, ['runner'], 60)
    proc(tmp_path, 501, 500, ['vendor-cli'], 59)
    wrapped = ("source /tmp/snap.sh 2>/dev/null || true && shopt -u extglob && eval 'gh run watch 77 --repo org/r' "
               "&& pwd -P >| /dev/shm/hive-run-3-x/claude-ab12-cwd")
    proc(tmp_path, 502, 501, ['/bin/bash', '-c', wrapped], 50)
    unit(tmp_path, 'hive-run-3', [500, 501, 502])
    running(conn, cid, 'hive-run-3', 500)
    gh = FakeGh(runs={77: {'name': 'CI', 'status': 'completed', 'conclusion': 'success'}})
    [head] = inspect.card(conn, cfg, cid, NOW, probe(tmp_path, gh))['heads']['heads']
    assert head['commands'][0]['cmdline'] == 'gh run watch 77 --repo org/r'
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
    assert d['destination'] == 'owner-inbox'
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
    (work / 'ev' / 'head.log').write_text(f'ran GH_TOKEN={V["ghp"]} gh pr list')
    assert inspect.read_file(cfg, c, 'ev/head.log')['text'] == 'ran GH_TOKEN=[REDACTED] gh pr list'
    assert [e['path'] for e in inspect.read_file(cfg, c, 'ev')['entries']] == ['ev/head.log', 'ev/log.txt']
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


def test_a_series_rule_is_off_board_with_its_schedule(env, tmp_path):
    cfg, conn, d = env
    rule = mk_series(conn, 'watcher', every=3600, max_runs=3)
    d.tick()
    snap = inspect.card(conn, cfg, rule, NOW, probe(tmp_path))
    assert snap['queue']['column'] == 'off_board' and snap['queue']['wait'] is None
    assert snap['series']['schedule']['every'] == 3600
    [run] = [i['id'] for i in api.get(conn, rule)['instances']]
    child = inspect.card(conn, cfg, run, NOW, probe(tmp_path))
    assert child['links']['series']['id'] == rule and child['queue']['column'] == 'off_board'


def test_health_counts_runs_today_and_hides_paths(env, tmp_path):
    cfg, conn, _ = env
    h = inspect.health(conn, cfg, NOW)
    assert all('runs_today' in v for v in h['vendors_today'].values())
    assert not any(isinstance(v, str) and v.startswith('/') for v in h['config']['budget'].values())
    assert h['config_mtime'] is not None


def test_a_budget_note_older_than_the_config_is_stale(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'waits for a vendor')
    conn.execute("UPDATE cards SET state='queued' WHERE id=?", (cid,))
    store.meta_set(conn, f'budget_wait:{cid}', {'v': 'daily runs limit reached'})
    os.utime(cfg['_path'], (NOW + 10, NOW + 10))
    bw = inspect.card(conn, cfg, cid, NOW + 20, probe(tmp_path))['queue']['budget_wait']
    assert bw['vendors'] == {'v': 'daily runs limit reached'} and bw['stale'] is True


def test_a_budget_note_older_than_the_last_launch_is_not_shown(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'waited for a vendor, then ran')
    conn.execute("UPDATE cards SET state='queued' WHERE id=?", (cid,))
    store.meta_set(conn, f'budget_wait:{cid}', {'v': 'daily runs limit reached'})
    _, at = store.meta_get(conn, f'budget_wait:{cid}')
    conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,ended_at)"
                 " VALUES(?,1,'worker','claude','m','e',1,'p','exited',?,?)", (cid, at + 1, at + 2))
    assert inspect.card(conn, cfg, cid, at + 20, probe(tmp_path))['queue']['budget_wait'] is None


def test_a_path_or_repository_is_not_a_unit_name(env, tmp_path):
    manager(tmp_path / 'cg')
    unit(tmp_path, 'web-app', [])
    p = probe(tmp_path)
    procs = [{'scan': 'cd /srv/web-app/repo; gh pr checks 1 --repo org/web-app', 'cmdline': '', 'cwd': None,
              'age_s': 1}]
    assert inspect.named_units(p, procs, 'hive-run-1') == []
    procs[0]['scan'] = 'until ! systemctl --user is-active web-app.service; do sleep 5; done'
    assert inspect.named_units(p, procs, 'hive-run-1') == ['web-app']


def test_a_quiet_series_result_is_kept_local(env, tmp_path):
    card = {'card_class': 'series_run', 'owner_visible': 0, 'series_stopped': None}
    done = {'state': 'done', 'result': {'news': False}}
    assert inspect.quiet_result(card, 'task_result', done)
    assert not inspect.quiet_result(card, 'task_result', {'state': 'done', 'result': {'news': True}})
    assert not inspect.quiet_result(dict(card, owner_visible=1), 'task_result', done)
    assert not inspect.quiet_result(card, 'task_blocked', done)


# ── the pulse's recent actions, the workspace's last commit and pull request, redaction ────────────────────────

def k(*parts):
    """A fake credential assembled at run time, so no key-shaped literal sits in the repository (ci/gitleaks_scan.py)."""
    return ''.join(parts)


# The fake credentials, each a value that must not survive into a snapshot.
V = {'bearer': k('abcdEFGH', '1234567890xyz'), 'header': k('ghp_', 'shortbutsecret'), 'basic': k('dXNlcjpw', 'YXNzd29yZA=='),
     'url': k('hunter2', 'pass'), 'git-url': k('s3cr3t', 'value99'), 'gh-token': k('tok_value', '_123456'),
     'api-key': k('quoted ', 'value 42'), 'aws': k('wJalrXUtnFEMIK7MDENG', 'bPxRfiCYzzzz'), 'password': k('plain', 'pw99'),
     'mysql': k('my', 'pw77'), 'flag-token': k('flag', 'token777'), 'flag-pw': k('flag', 'pw888'),
     'json-pw': k('json', 'pw555'), 'json-key': k('json', 'key444'),
     'ghp': k('gh', 'p_', 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789'),
     'pat': k('github', '_pat_', '11ABCDEFG0123456789_abcdefghijklmnopqrstuv'),
     'sk': k('sk-', 'ant-api03-', 'AbCdEfGhIjKlMnOpQrStUvWxYz012345'), 'sk-proj': k('sk-', 'proj-', 'ZyXwVuTsRqPoNmLkJiHgFeDcBa'),
     'slack': k('xo', 'xb-', '123456789012-abcdefghijkl'), 'akia': k('AK', 'IA', 'IOSFODNN7EXAMPLE'),
     'jwt': k('eyJhbGciOiJIUzI1NiJ9', '.', 'eyJzdWIiOiIxMjM0NTY3ODkwIn0', '.', 'dozjgNryP4J3jVmNHl0w5N'),
     'telegram': k('123456789', ':', 'AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw'), 'key-block': k('b3BlbnNz', 'aC1rZXk')}
SECRETS = {
    'bearer': f'curl -H "Authorization: Bearer {V["bearer"]}" https://api.example.com/x',
    'header': f"curl -H 'Authorization: token {V['header']}' https://api.github.com",
    'basic': f'curl -H "authorization: Basic {V["basic"]}" https://h',
    'url': f'psql postgres://admin:{V["url"]}@db.internal:5432/app -c "select 1"',
    'git-url': f'git push https://x-access-token:{V["git-url"]}@github.com/org/repo.git',
    'gh-token': f'GH_TOKEN={V["gh-token"]} gh pr list',
    'api-key': f'OPENAI_API_KEY="{V["api-key"]}" python run.py',
    'aws': f'AWS_SECRET_ACCESS_KEY={V["aws"]} aws s3 ls',
    'password': f'PGPASSWORD={V["password"]} psql -h db',
    'mysql': f'MYSQL_PASSWORD={V["mysql"]} mysql',
    'flag': f'deploy --token {V["flag-token"]} --password={V["flag-pw"]}',
    'json': f"""echo '{{"password": "{V['json-pw']}", "api_key": "{V['json-key']}"}}'""",
    'ghp': f'echo {V["ghp"]}',
    'pat': f'echo {V["pat"]}',
    'sk': f'export X={V["sk"]}',
    'sk-proj': f'echo {V["sk-proj"]}',
    'slack': f'echo {V["slack"]}',
    'akia': f'echo {V["akia"]}',
    'jwt': f'echo {V["jwt"]}',
    'telegram': f'curl https://api.telegram.org/bot{V["telegram"]}/getMe',
    'key-block': 'echo "' + k('-----BEGIN OPENSSH ', 'PRIVATE KEY-----') + '\n' + V['key-block'] + '\n'
                 + k('-----END OPENSSH ', 'PRIVATE KEY-----') + '"',
}
LEAKS = list(V.values())


@pytest.mark.parametrize('name', sorted(SECRETS))
def test_redact_masks_every_credential_form(name):
    out = inspect.redact(SECRETS[name])
    assert inspect.MASK in out
    assert not [leak for leak in LEAKS if leak in out], out


def test_redact_keeps_ordinary_command_lines():
    for line in ('gh pr checks 42 -R org/repo --watch', 'pytest -q tests/hive/test_inspect.py -k redact',
                 'git log -1 --format=%H', 'sort --key=2 file.txt', 'curl https://example.com/path?q=1'):
        assert inspect.redact(line) == line


def claude_line(at, *content, parent=None):
    return json.dumps({'type': 'assistant', 'timestamp': at, 'session_id': 's', 'parent_tool_use_id': parent,
                       'message': {'content': list(content)}})


def test_pulse_lists_a_heads_recent_actions_redacted(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'works')
    manager(tmp_path / 'cg')
    secret_loop = f'GH_TOKEN={V["ghp"]} gh pr checks 42 --watch'
    proc(tmp_path, 500, 1, ['runner'], 600)
    proc(tmp_path, 501, 500, ['vendor-cli'], 590)
    proc(tmp_path, 502, 501, ['/bin/bash', '-c', secret_loop], 100)
    proc(tmp_path, 503, 502, ['curl', '-H', 'Authorization: Bearer ' + V['bearer'], 'https://h'], 2)
    unit(tmp_path, 'hive-run-5', [500, 501, 502, 503])
    rid = running(conn, cid, 'hive-run-5', 500)
    run_dir = tmp_path / 'ws' / '.hive' / 'r1-worker-claude-x'
    run_dir.mkdir(parents=True)
    lines = ['not json: a vendor warning', json.dumps({'type': 'system', 'subtype': 'init', 'session_id': 's'})]
    for i in range(12):
        lines.append(claude_line(f'2033-05-18T03:{i:02d}:00.000Z',
                                 {'type': 'tool_use', 'name': 'Bash', 'input': {'command': f'step {i}'}}))
    lines.append(claude_line('2033-05-18T03:20:00.000Z', {'type': 'thinking', 'thinking': 'hidden'},
                             {'type': 'text', 'text': 'Now pushing with PGPASSWORD=' + V['password']}))
    lines.append(claude_line('2033-05-18T03:21:00.000Z', {'type': 'tool_use', 'name': 'Bash', 'input': {
        'command': SECRETS['bearer'].replace('api.example.com/x', 'api')}}))
    lines.append(claude_line('2033-05-18T03:22:00.000Z', {'type': 'tool_use', 'name': 'Read', 'input': {
        'file_path': '/x/sub.py'}}, parent='toolu_sub'))
    lines.append(json.dumps({'type': 'user', 'message': {'content': [{'type': 'tool_result', 'content': 'output'}]}}))
    (run_dir / 'head.log').write_text('\n'.join(lines) + '\n')
    conn.execute("UPDATE runs SET vendor='claude', run_dir=? WHERE id=?", (str(run_dir), rid))
    gh = FakeGh(pulls={42: {'number': 42, 'state': 'OPEN', 'statusCheckRollup': []}})
    snap = inspect.card(conn, cfg, cid, NOW, probe(tmp_path, gh))
    [head] = snap['heads']['heads']
    acts = head['actions']
    assert len(acts) == inspect.ACTIONS
    assert [a['text'] for a in acts[:2]] == ['Bash: step 4', 'Bash: step 5']
    assert acts[-2] == {'at': 1999999200.0, 'kind': 'text', 'text': 'Now pushing with PGPASSWORD=[REDACTED]'}
    assert acts[-1]['text'] == 'Bash: curl -H "Authorization: Bearer [REDACTED]" https://api'
    assert head['log']['size'] > 0
    assert head['waiting_on']['command'] == '/bin/bash -c GH_TOKEN=[REDACTED] gh pr checks 42 --watch'
    assert head['waiting_on']['now'].startswith('curl -H Authorization: Bearer [REDACTED]')
    assert head['pull_requests'][0]['pr'] == 42
    dumped = json.dumps(snap)
    assert not [leak for leak in LEAKS if leak in dumped]
    assert 'hidden' not in dumped and 'sub.py' not in dumped


def test_recent_actions_read_a_codex_stream(tmp_path):
    lines = [json.dumps({'method': 'item/started', 'emittedAtMs': 2_000_000_000_000,
                         'params': {'item': {'type': 'commandExecution', 'command': 'pytest -q'}}}),
             json.dumps({'method': 'item/completed', 'emittedAtMs': 2_000_000_001_000,
                         'params': {'item': {'type': 'commandExecution', 'command': 'pytest -q'}}}),
             json.dumps({'method': 'item/completed', 'emittedAtMs': 2_000_000_002_000,
                         'params': {'item': {'type': 'agentMessage', 'text': 'tests pass'}}}),
             json.dumps({'type': 'item.started', 'item': {'type': 'file_change', 'changes': [{'path': 'a.py'}]}})]
    (tmp_path / 'head.log').write_text('\n'.join(lines))
    got = inspect.recent_actions(tmp_path, 'codex')['actions']
    assert [(a['at'], a['text']) for a in got] == [(2_000_000_000.0, 'shell: pytest -q'),
                                                   (2_000_000_002.0, 'tests pass'), (None, 'edit: a.py')]
    assert inspect.recent_actions(tmp_path / 'none', 'codex') == {'actions': [], 'log': None}


def test_recent_actions_read_only_the_tail_of_a_long_log(tmp_path):
    old = claude_line('2033-05-18T03:00:00Z', {'type': 'tool_use', 'name': 'Bash', 'input': {'command': 'first'}})
    pad = json.dumps({'type': 'user', 'pad': 'x' * 1000})
    last = claude_line('2033-05-18T04:00:00Z', {'type': 'tool_use', 'name': 'Bash', 'input': {'command': 'last'}})
    (tmp_path / 'head.log').write_text('\n'.join([old] + [pad] * 600 + [last]) + '\n')
    got = inspect.recent_actions(tmp_path, 'claude')['actions']
    assert [a['text'] for a in got] == ['Bash: last']


def test_unwrap_strips_both_vendor_shell_wrappers():
    snap = ("/bin/bash -c source /srv/u/.claude/shell-snapshots/snapshot-bash-1.sh 2>/dev/null || true && "
            "shopt -u extglob 2>/dev/null || true && eval 'gh pr checks 7 --watch; echo '\"'\"'done'\"'\"'' "
            "&& pwd -P >| /dev/shm/hive-run-1-x/claude-0de0-cwd")
    assert inspect.unwrap(snap) == "gh pr checks 7 --watch; echo 'done'"
    # the wrapper as a live head's process showed it (h_c3974085, 26.11)
    case = ("/bin/bash -c { case \"${BASH_VERSINFO-}\" in '2'|'3') \\builtin printf '%s\\n' "
            "'__CLAUDE_CODE_DID_NOT_RUN_BASH_TOO_OLD='\"${BASH_VERSION-}\" >&2; \\builtin exit 126; \\exit 126;; esac; } "
            "&& source /srv/u/.claude/shell-snapshots/snapshot-bash-1.sh 2>/dev/null || true && shopt -u extglob "
            "2>/dev/null || true && { \\builtin unalias -- 'which'; \\builtin unset -f -- 'which'; } >/dev/null 2>&1 "
            "|| true && eval 'make test' && pwd -P >| /tmp/claude-1-cwd")
    assert inspect.unwrap(case) == 'make test'
    # a command the head runs in the background reads /dev/null (h_b64145d3, 27.11)
    background = ("/bin/bash -c source /srv/u/.claude/shell-snapshots/snapshot-bash-1.sh 2>/dev/null || true && "
                  "eval 'while kill -0 42 2>/dev/null; do sleep 60; done; date' < /dev/null "
                  "&& pwd -P >| /dev/shm/hive-run-1-x/claude-35b5-cwd")
    assert inspect.unwrap(background) == 'while kill -0 42 2>/dev/null; do sleep 60; done; date'
    assert inspect.unwrap('/bin/bash -c make test') == '/bin/bash -c make test'


def git_repo(path, branch):
    import subprocess
    run = lambda *a: subprocess.run(['git', '-C', str(path), *a], check=True, capture_output=True)  # noqa: E731
    path.mkdir(parents=True)
    run('init', '-q', '-b', branch)
    run('config', 'user.email', 't@example.com')
    run('config', 'user.name', 'T')
    run('remote', 'add', 'origin', f'https://x-access-token:{V["git-url"]}@github.com/org/repo.git')
    (path / 'f').write_text('1')
    run('add', 'f')
    run('commit', '-q', '-m', f'hive: add f with GH_TOKEN={V["gh-token"]}')


def test_pulse_names_the_workspaces_last_commit_branch_and_pull_request(env, tmp_path):
    cfg, conn, _ = env
    cid = mk(conn, 'commits')
    work = tmp_path / 'ws'
    git_repo(work / 'repo', 'hive/feature')
    (work / 'plain').mkdir()
    conn.execute('UPDATE cards SET workdir=? WHERE id=?', (str(work), cid))
    p = probe(tmp_path)
    calls = []
    p.branch_pull = lambda branch, repo, cwd: (calls.append((branch, repo)), {
        'number': 9, 'url': 'https://github.com/org/repo/pull/9', 'state': 'OPEN', 'title': 't', 'updatedAt': 'u',
        'mergedAt': None})[1]
    snap = inspect.card(conn, cfg, cid, NOW, p)
    [repo] = snap['heads']['repos']
    assert (repo['path'], repo['branch'], repo['repo'], repo['dirty']) == ('repo', 'hive/feature', 'org/repo', False)
    assert repo['last_commit']['subject'] == 'hive: add f with GH_TOKEN=[REDACTED]'
    assert len(repo['last_commit']['sha']) == 12 and repo['last_commit']['at'] > 0
    assert repo['remote'] == 'https://[REDACTED]@github.com/org/repo.git'
    assert repo['pull_request']['number'] == 9 and calls == [('hive/feature', 'org/repo')]
    (work / 'repo' / 'f').write_text('2')
    assert inspect.card(conn, cfg, cid, NOW, p)['heads']['repos'][0]['dirty'] is True
    assert not [leak for leak in LEAKS if leak in json.dumps(snap)]
