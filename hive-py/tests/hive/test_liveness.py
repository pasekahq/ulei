"""A run lives while its process does: the probe on a fake cgroup tree, reaping, the break context and stuck."""
import json
import os
import subprocess
import sys
from pathlib import Path

import pytest
from test_fake_inbox import C1, violations
from test_hive import env, fake_time, mk, record_spawns  # noqa: F401
from hive import api, liveness, store

UNIT = 'hive-run-7'


def unit_dir(root, unit):
    uid = os.getuid()
    return root / 'user.slice' / f'user-{uid}.slice' / f'user@{uid}.service' / 'app.slice' / f'{unit}.service'


def manager(root):
    unit_dir(root, UNIT).parent.mkdir(parents=True)


def start_unit(root, unit, populated=1):
    path = unit_dir(root, unit)
    path.mkdir(parents=True, exist_ok=True)
    (path / 'cgroup.events').write_text(f'populated {populated}\nfrozen 0\n')


@pytest.mark.parametrize('populated,expected', [(1, True), (0, False)])
def test_probe_reads_populated_of_the_unit_cgroup(tmp_path, populated, expected):
    start_unit(tmp_path, UNIT, populated)
    assert liveness.alive({'unit': UNIT, 'pid': None}, tmp_path) is expected


def test_probe_counts_a_missing_unit_cgroup_as_gone(tmp_path):
    manager(tmp_path)
    assert liveness.alive({'unit': UNIT, 'pid': os.getpid()}, tmp_path) is False


def test_probe_refuses_to_judge_without_the_user_manager_cgroup(tmp_path):
    with pytest.raises(FileNotFoundError, match='user manager'):
        liveness.alive({'unit': UNIT, 'pid': None}, tmp_path)


def test_probe_of_a_popen_run_reads_its_pid(tmp_path):
    ended = subprocess.Popen([sys.executable, '-c', ''])
    ended.wait()
    assert liveness.alive({'unit': None, 'pid': os.getpid()}, tmp_path) is True
    assert liveness.alive({'unit': None, 'pid': ended.pid}, tmp_path) is False
    assert liveness.alive({'unit': None, 'pid': None}, tmp_path) is False


@pytest.fixture
def world(env, tmp_path, monkeypatch):  # noqa: F811
    """The env dispatcher on a fixed clock with recorded launches and a fake cgroup tree."""
    cfg, conn, d = env
    now = fake_time(monkeypatch, d)
    record_spawns(d)
    d.cgroup_root = tmp_path / 'cgroup'
    manager(d.cgroup_root)
    return cfg, conn, d, now


def latest(conn, cid):
    return conn.execute("SELECT * FROM runs WHERE card_id=? AND role='worker' ORDER BY id DESC LIMIT 1",
                        (cid,)).fetchone()


def claim(conn, d, cid, log_lines=0):
    """The runner of the latest worker run claims it in its unit, heartbeats once and writes `log_lines` to head.log."""
    run = latest(conn, cid)
    unit = f"hive-run-{run['id']}"
    conn.execute("UPDATE runs SET status='running', unit=?, heartbeat_at=? WHERE id=?", (unit, d.clock(), run['id']))
    start_unit(d.cgroup_root, unit)
    run_dir = Path(run['run_dir'])
    lines = [f'log line {i:03d}' for i in range(1, log_lines + 1)]
    if run['vendor'] == 'codex':
        (run_dir / 'last-message.txt').write_text('last words\n')
    else:  # the last message of a stream-json head is its result line
        lines.insert(0, json.dumps({'type': 'result', 'result': 'last words'}))
    (run_dir / 'head.log').write_text('\n'.join(lines) + '\n')
    return latest(conn, cid)


def gone(d, run):
    """The run's unit ended: the user manager removed its cgroup."""
    path = unit_dir(d.cgroup_root, run['unit'])
    (path / 'cgroup.events').unlink()
    path.rmdir()


def exited_with_report(conn, run):
    with store.tx(conn):
        conn.execute("UPDATE runs SET status='exited', exit_code=0, ended_at=?, report='RESULT: partial' WHERE id=?",
                     (run['started_at'], run['id']))
        store.event(conn, run['card_id'], 'run_report', {'run': run['id'], 'file': 'REPORT.md', 'source': 'file',
                                                         'inherited': None}, actor='runner')


def test_live_process_with_a_stale_heartbeat_stays_running(world):
    cfg, conn, d, now = world
    cid = mk(conn, '[ok] long quiet tool call')
    d.tick()
    run = claim(conn, d, cid)
    conn.execute('UPDATE runs SET heartbeat_at=? WHERE id=?', (now[0] - 100000, run['id']))
    now[0] += cfg['timeouts'][1]
    d.tick()
    assert latest(conn, cid)['status'] == 'running' and api.get(conn, cid)['state'] == 'running'
    assert 'run_reaped' not in [e['kind'] for e in api.get(conn, cid)['events']]


def test_starting_run_without_a_process_is_reaped_only_after_the_spawn_grace(world):
    cfg, conn, d, now = world
    cid = mk(conn, '[ok] runner slow to start')
    d.tick()
    conn.execute('UPDATE runs SET unit=? WHERE id=?', (UNIT, latest(conn, cid)['id']))
    now[0] += cfg['spawn_grace_s']
    assert d.reap() == 0
    now[0] += 1
    assert d.reap() == 1
    assert latest(conn, cid)['error'] == 'crashed: head never started'


def test_gone_process_without_a_result_starts_a_round_with_the_break_context(world):
    _, conn, d, now = world
    cid = mk(conn, '[ok] head killed mid-work')
    d.tick()
    first = claim(conn, d, cid, log_lines=60)
    gone(d, first)
    now[0] += 1
    d.tick()
    first = conn.execute('SELECT * FROM runs WHERE id=?', (first['id'],)).fetchone()
    assert (first['status'], first['error']) == ('crashed', 'crashed: process gone without a result')
    card = api.get(conn, cid)
    assert card['state'] == 'running' and card['round'] == 2
    assert [e['data']['reason'] for e in card['events'] if e['kind'] == 'retry'] == [
        'worker crashed: process gone without a result']
    prompt = latest(conn, cid)['prompt']
    assert f"Run {first['id']} (worker) ended without a result: crashed: process gone without a result." in prompt
    assert 'log line 060' in prompt and 'log line 041' in prompt and 'log line 040' not in prompt
    assert 'Last lines of its last message:\nlast words' in prompt


@pytest.mark.parametrize('second', ['gone', 'timeout'])
def test_second_round_in_a_row_without_a_result_is_stuck_with_one_secretary_event(world, second):
    cfg, conn, d, now = world
    cfg['max_rounds'] = 100
    killed = []
    d.kill = lambda r: killed.append(r['id'])
    cid = mk(conn, '[ok] head keeps dying')
    d.tick()
    first = claim(conn, d, cid, log_lines=3)
    gone(d, first)
    d.tick()
    run = claim(conn, d, cid, log_lines=3)
    if second == 'gone':
        gone(d, run)
    else:
        now[0] += cfg['timeouts'][1] + 121
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'stuck')
    assert killed == ([run['id']] if second == 'timeout' else [])
    assert len(card['runs']) == 2 and len([e for e in card['events'] if e['kind'] == 'retry']) == 1
    [row] = conn.execute('SELECT * FROM outbox WHERE card_id=?', (cid,)).fetchall()
    assert row['kind'] == 'task_blocked' and row['published_at'] is not None
    packet = d.inbox().packets[row['event_id']]
    assert violations(packet, C1, C1) == []
    payload = json.loads(packet['payload'])
    reason = 'crashed: process gone without a result' if second == 'gone' else f"timeout: running {cfg['timeouts'][1] + 121}s > {cfg['timeouts'][1]}s"
    assert payload['state'] == 'blocked'
    assert payload['reason'] == (f"stuck: 2 rounds in a row ended without a result (runs {first['id']}, {run['id']}); "
                                 f'last: worker {reason}')
    assert payload['stuck'] == [
        {'run': first['id'], 'role': 'worker', 'reason': 'crashed: process gone without a result'},
        {'run': run['id'], 'role': 'worker', 'reason': reason}]
    [(column, entry)] = [(name, e) for name, entries in api.board(conn, now[0])['columns'].items()
                         for e in entries if e['id'] == cid]
    assert (column, entry['badge'], entry['cause'], entry['reason']) == ('waiting', 'external', 'stuck', payload['reason'])


def test_round_with_a_result_resets_the_count(world):
    cfg, conn, d, _ = world
    cfg['max_rounds'] = 100
    cid = mk(conn, '[ok] one crash, one gate failure, one crash')
    d.tick()
    gone(d, claim(conn, d, cid))
    d.tick()
    exited_with_report(conn, claim(conn, d, cid))
    d.tick()
    gone(d, claim(conn, d, cid))
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason'], card['round']) == ('running', None, 4)
    assert [e['data']['reason'] for e in card['events'] if e['kind'] == 'retry'] == [
        'worker crashed: process gone without a result', 'gate exit 1', 'worker crashed: process gone without a result']
    assert conn.execute('SELECT count(*) FROM outbox WHERE card_id=?', (cid,)).fetchone()[0] == 0
