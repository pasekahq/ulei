"""A run lives while its process does: the probe on a fake cgroup tree, reaping, the break context, stuck, and the
wait on processes a head left running."""
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from test_fake_inbox import C1, violations
from test_hive import env, fake_time, mk, record_spawns  # noqa: F401
from hive import api, liveness, runner, store

UNIT = 'hive-run-7'
RUNNER_PID = 4000
LEFTOVERS = {4001: b'sleep\x00600\x00', 4002: b'make\x00-j2\x00'}


APP = 'app.slice'
NESTED = 'parent.slice/parent-child.slice'  # a drop-in's `Slice=parent-child.slice`


def unit_dir(root, unit, slice_=APP):
    uid = os.getuid()
    return root / 'user.slice' / f'user-{uid}.slice' / f'user@{uid}.service' / slice_ / f'{unit}.service'


def manager(root):
    unit_dir(root, UNIT).parent.mkdir(parents=True)


def start_unit(root, unit, populated=1, slice_=APP):
    path = unit_dir(root, unit, slice_)
    path.mkdir(parents=True, exist_ok=True)
    (path / 'cgroup.events').write_text(f'populated {populated}\nfrozen 0\n')


@pytest.mark.parametrize('slice_', [APP, 'other.slice', NESTED])
@pytest.mark.parametrize('populated,expected', [(1, True), (0, False)])
def test_probe_reads_populated_of_the_unit_cgroup(tmp_path, populated, expected, slice_):
    manager(tmp_path)
    start_unit(tmp_path, UNIT, populated, slice_)
    assert liveness.alive({'unit': UNIT, 'pid': None}, tmp_path) is expected


def test_probe_finds_the_unit_only_in_a_slice(tmp_path):
    """A same-named directory under a service or a scope is no unit of the user manager."""
    manager(tmp_path)
    start_unit(tmp_path, UNIT, slice_='parent.slice/other.service')
    start_unit(tmp_path, UNIT, slice_='init.scope')
    assert liveness.unit_cgroup(tmp_path, UNIT) is None
    assert liveness.alive({'unit': UNIT, 'pid': None}, tmp_path) is False
    start_unit(tmp_path, UNIT, slice_=NESTED)
    assert liveness.unit_cgroup(tmp_path, UNIT) == unit_dir(tmp_path, UNIT, NESTED)


def test_probe_counts_a_missing_unit_cgroup_as_gone(tmp_path):
    manager(tmp_path)
    assert liveness.alive({'unit': UNIT, 'pid': os.getpid()}, tmp_path) is False


def test_probe_refuses_to_judge_without_the_user_manager_cgroup(tmp_path):
    with pytest.raises(FileNotFoundError, match='user manager'):
        liveness.alive({'unit': UNIT, 'pid': None}, tmp_path)


def hold(cgroup_root, proc_root, unit, pids, cmdlines, slice_=APP):
    """The unit cgroup lists `pids`; each pid of `cmdlines` has that /proc cmdline."""
    (unit_dir(cgroup_root, unit, slice_) / 'cgroup.procs').write_text(''.join(f'{pid}\n' for pid in pids))
    for pid, cmdline in cmdlines.items():
        (proc_root / str(pid)).mkdir(parents=True, exist_ok=True)
        (proc_root / str(pid) / 'cmdline').write_bytes(cmdline)


@pytest.mark.parametrize('slice_', [APP, NESTED])
def test_leftovers_are_the_unit_processes_other_than_the_runner(tmp_path, slice_):
    cgroup, proc = tmp_path / 'cgroup', tmp_path / 'proc'
    start_unit(cgroup, UNIT, slice_=slice_)
    hold(cgroup, proc, UNIT, [RUNNER_PID, 4001, 4002, 4003, 4004], {RUNNER_PID: b'python3\x00', **LEFTOVERS, 4003: b''},
         slice_)
    run = {'unit': UNIT, 'pid': RUNNER_PID}
    assert liveness.leftovers(run, cgroup, proc) == [{'pid': 4001, 'cmdline': 'sleep 600'},
                                                     {'pid': 4002, 'cmdline': 'make -j2'}]
    shutil.rmtree(unit_dir(cgroup, UNIT, slice_))
    assert liveness.leftovers(run, cgroup, proc) == []
    assert liveness.leftovers({'unit': None, 'pid': RUNNER_PID}, cgroup, proc) == []


class FakeStop:
    """The runner's stop event without sleeping: wait number `n` ends the leftovers when n == end_at and reports a
    stop when n == stop_at."""
    def __init__(self, end_leftovers, end_at=None, stop_at=None):
        self.end_leftovers, self.end_at, self.stop_at, self.waits = end_leftovers, end_at, stop_at, []

    def wait(self, timeout):
        self.waits.append(timeout)
        if len(self.waits) == self.end_at:
            self.end_leftovers()
        return len(self.waits) == self.stop_at


@pytest.mark.parametrize('leftovers,end_at,stop_at,waits', [
    ({}, None, None, 0), (LEFTOVERS, 2, None, 2), (LEFTOVERS, None, 1, 1)])
def test_runner_stays_in_its_unit_until_the_leftovers_end_or_a_stop(tmp_path, monkeypatch, leftovers, end_at, stop_at, waits):
    cgroup, proc = tmp_path / 'cgroup', tmp_path / 'proc'
    monkeypatch.setattr(liveness, 'CGROUP_ROOT', cgroup)
    monkeypatch.setattr(liveness, 'PROC_ROOT', proc)
    start_unit(cgroup, UNIT)
    hold(cgroup, proc, UNIT, [os.getpid(), *leftovers], leftovers)
    stopping = FakeStop(lambda: hold(cgroup, proc, UNIT, [os.getpid()], {}), end_at, stop_at)
    runner.outlive_leftovers(UNIT, stopping)
    assert stopping.waits == [runner.HEARTBEAT_S] * waits


def test_probe_of_a_popen_run_reads_its_pid(tmp_path):
    ended = subprocess.Popen([sys.executable, '-c', ''])
    ended.wait()
    assert liveness.alive({'unit': None, 'pid': os.getpid()}, tmp_path) is True
    assert liveness.alive({'unit': None, 'pid': ended.pid}, tmp_path) is False
    assert liveness.alive({'unit': None, 'pid': None}, tmp_path) is False


@pytest.fixture
def world(env, tmp_path, monkeypatch):  # noqa: F811
    """The env dispatcher on a fixed clock with recorded launches and a fake cgroup and /proc tree."""
    cfg, conn, d = env
    now = fake_time(monkeypatch, d)
    record_spawns(d)
    d.cgroup_root = tmp_path / 'cgroup'
    d.proc_root = tmp_path / 'proc'
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
    shutil.rmtree(unit_dir(d.cgroup_root, run['unit']))


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
    packet = d.adapter().inbox.packets[row['event_id']]
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


def leave(conn, d, run, leftovers):
    """The run's runner (RUNNER_PID) recorded an exit with a report while its unit holds `leftovers` besides it."""
    conn.execute('UPDATE runs SET pid=? WHERE id=?', (RUNNER_PID, run['id']))
    hold(d.cgroup_root, d.proc_root, run['unit'], [RUNNER_PID, *leftovers], leftovers)
    exited_with_report(conn, run)
    return conn.execute('SELECT * FROM runs WHERE id=?', (run['id'],)).fetchone()


def placed(conn, now, cid):
    [(column, entry)] = [(name, e) for name, entries in api.board(conn, now)['columns'].items()
                         for e in entries if e['id'] == cid]
    return column, entry


def test_exited_worker_with_leftovers_waits_on_them_untouched_by_its_timeout(world):
    cfg, conn, d, now = world
    killed = []
    d.kill = lambda r: killed.append(r['id'])
    cid = mk(conn, '[ok] head starts a long build and exits')
    d.tick()
    run = leave(conn, d, claim(conn, d, cid), LEFTOVERS)
    d.tick()
    now[0] += cfg['timeouts'][1] + 10000
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason'], len(card['runs']), card['gates']) == ('blocked', 'process', 1, [])
    assert [e['data'] for e in card['events'] if e['kind'] == 'process_wait'] == [
        {'run': run['id'], 'round': 1, 'processes': [{'pid': 4001, 'cmdline': 'sleep 600'}, {'pid': 4002, 'cmdline': 'make -j2'}]}]
    column, entry = placed(conn, now[0], cid)
    assert (column, entry['badge'], entry['cause'], entry['reason']) == (
        'waiting', 'external', 'process', f"run {run['id']} left 2 process(es) running: 4001 sleep 600; 4002 make -j2")
    assert killed == [] and conn.execute('SELECT count(*) FROM outbox WHERE card_id=?', (cid,)).fetchone()[0] == 0


def test_ended_leftovers_give_a_new_round_with_their_context_and_no_failure(world):
    cfg, conn, d, now = world
    cfg['retry_backoff'] = {'base_s': 600, 'max_s': 3600}
    cid = mk(conn, '[ok] head waits for its build')
    d.tick()
    first = leave(conn, d, claim(conn, d, cid), LEFTOVERS)
    d.tick()
    now[0] += 600
    d.tick()
    assert api.get(conn, cid)['state'] == 'blocked'
    hold(d.cgroup_root, d.proc_root, first['unit'], [RUNNER_PID], {})
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['round'], card['retry_streak'], card['wait_reason']) == ('running', 2, 0, None)
    assert [e for e in card['events'] if e['kind'] == 'retry'] == []
    assert [e['data'] for e in card['events'] if e['kind'] == 'process_ended'] == [
        {'run': first['id'], 'round': 1, 'at': now[0]}]
    prompt = latest(conn, cid)['prompt']
    assert '## The processes round 1 left running have ended\n' in prompt and 'Previous round failed' not in prompt
    assert (f"Run {first['id']} (worker) exited at 1970-01-01T00:16:40Z and left 2 process(es) running; "
            'all of them had ended by 1970-01-01T00:26:40Z:\n- pid 4001: sleep 600\n- pid 4002: make -j2\n\n'
            'Last lines of its last message:\nlast words') in prompt


def test_waited_round_leaves_the_stuck_count_unchanged(world):
    cfg, conn, d, _ = world
    cfg['max_rounds'] = 100
    cid = mk(conn, '[ok] crash, wait on a build, crash')
    d.tick()
    first = claim(conn, d, cid)
    gone(d, first)
    d.tick()
    waited = leave(conn, d, claim(conn, d, cid), LEFTOVERS)
    d.tick()
    gone(d, waited)
    d.tick()
    third = claim(conn, d, cid)
    gone(d, third)
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'stuck')
    assert card['block_reason'].startswith(f"stuck: 2 rounds in a row ended without a result (runs {first['id']}, {third['id']})")


def test_cancel_of_a_card_waiting_on_leftovers_stops_its_unit(world):
    _, conn, d, _ = world
    stopped = []

    def kill(r):
        stopped.append(r['unit'])
        gone(d, r)
    d.kill = kill
    cid = mk(conn, '[ok] head leaves a server running')
    d.tick()
    run = leave(conn, d, claim(conn, d, cid), LEFTOVERS)
    d.tick()
    assert stopped == []
    assert d.cancel(cid, 'not needed') is True
    assert stopped == [run['unit']] and api.get(conn, cid)['state'] == 'cancelled'


@pytest.mark.parametrize('stop', ['reaper', 'runner'])
def test_two_timeouts_in_a_row_are_stuck_whatever_the_runner_recorded(world, stop):
    cfg, conn, d, now = world
    cfg['max_rounds'] = 100
    stopped = []

    def sigterm_record(r):
        """The runner catches the stop's SIGTERM and records an exit with a report before its unit ends."""
        stopped.append(r['id'])
        exited_with_report(conn, r)
        conn.execute("UPDATE runs SET error='stopped by SIGTERM' WHERE id=?", (r['id'],))
        gone(d, r)
    d.kill = sigterm_record
    cid = mk(conn, '[ok] head hangs every round')
    d.tick()
    runs = []
    for _ in range(2):
        run = claim(conn, d, cid)
        runs.append(run['id'])
        if stop == 'reaper':
            now[0] += cfg['timeouts'][1] + 121
        else:
            conn.execute("""UPDATE runs SET status='timeout', ended_at=?, outcome='timeout', error=? WHERE id=?""",
                         (now[0], f"timeout after {cfg['timeouts'][1]}s", run['id']))
            gone(d, run)
        d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'stuck')
    assert [r['status'] for r in card['runs']] == ['timeout', 'timeout']
    assert stopped == (runs if stop == 'reaper' else [])
    assert len([e for e in card['events'] if e['kind'] == 'retry']) == 1


def test_exit_with_a_result_and_no_leftovers_goes_to_the_gate(world):
    _, conn, d, _ = world
    cid = mk(conn, '[ok] head exits alone')
    d.tick()
    leave(conn, d, claim(conn, d, cid), {})
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['round']) == ('running', 2)
    assert [e['data']['reason'] for e in card['events'] if e['kind'] == 'retry'] == ['gate exit 1']
    assert [e for e in card['events'] if e['kind'] == 'process_wait'] == []
