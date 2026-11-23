"""Real subprocess execution without heads, restart safety, failures and timeout."""
import json
from pathlib import Path

import pytest
from test_hive import env, fake_time, run_until  # noqa: F401
from hive import api, dispatcher, runner


def create(conn, tmp_path, body='printf hello', **kwargs):
    path = tmp_path / 'job.sh'
    path.write_text(body)
    return api.create(conn, title='script', goal='check', acceptance=kwargs.pop('acceptance', None), step=1,
                      source='system:test', created_by='secretary', script=str(path), deliver='local', **kwargs)[0]


def test_stdout_result_and_exit_zero_acceptance_no_model(env, tmp_path, monkeypatch):
    _, conn, d = env
    monkeypatch.setattr('hive.heads.weekly_left', lambda *a: pytest.fail('script requested model budget'))
    cid = create(conn, tmp_path, 'printf hello; printf stderr-only >&2')
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    r = c['runs'][0]
    assert (c['result'], r['report'], r['exit_code'], r['vendor']) == ('hello', 'hello', 0, 'script')
    assert r['tokens_in'] is None and r['tokens_out'] is None
    assert (Path(r['run_dir']) / 'stderr.log').read_text() == 'stderr-only'
    job = json.loads((Path(r['run_dir']) / 'job.json').read_text())
    assert 'env' not in job and 'TELEGRAM_BOT_TOKEN' not in json.dumps(job)


def test_nonzero_exit_alert_state_no_automatic_rerun(env, tmp_path):
    _, conn, d = env
    cid = create(conn, tmp_path, 'printf diagnostic; exit 7')
    assert run_until(d, conn, cid) == 'blocked'
    d.tick()
    c = api.get(conn, cid)
    assert len(c['runs']) == 1 and c['runs'][0]['exit_code'] == 7
    assert 'exit=7' in c['block_reason']


def test_script_timeout(env, tmp_path):
    _, conn, d = env
    cid = create(conn, tmp_path, 'sleep 90', script_timeout=0.2)
    assert run_until(d, conn, cid) == 'blocked'
    assert api.get(conn, cid)['runs'][0]['status'] == 'timeout'


def test_custom_gate_does_not_retry_script(env, tmp_path):
    _, conn, d = env
    cid = create(conn, tmp_path, 'printf hello', acceptance='false')
    assert run_until(d, conn, cid) == 'blocked'
    assert len(api.get(conn, cid)['runs']) == 1


def test_duplicate_runner_claim_and_dispatcher_restart(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    counter = tmp_path / 'counter'
    cid = create(conn, tmp_path, f'echo ran >> {counter}')
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    d.tick()
    run = api.get(conn, cid)['runs'][0]
    assert runner.main(['--db', cfg['db'], '--run-id', str(run['id'])]) == 0
    assert runner.main(['--db', cfg['db'], '--run-id', str(run['id'])]) == 3
    dispatcher.Dispatcher(cfg, conn).tick()
    assert counter.read_text() == 'ran\n'
    assert api.get(conn, cid)['state'] == 'done'


def test_systemd_transient_spawn_unit(env, tmp_path, monkeypatch):
    _, conn, d = env
    cid = create(conn, tmp_path)
    d.launcher = 'systemd'
    calls = []
    monkeypatch.setattr(dispatcher.subprocess, 'run', lambda argv, **kw: calls.append(argv))
    d.tick()
    r = api.get(conn, cid)['runs'][0]
    assert r['unit'] == f"hive-run-{r['id']}"
    assert calls[0][0] == 'systemd-run' and '--collect' in calls[0] and 'hive.runner' in calls[0]


@pytest.mark.parametrize('kwargs', [{'script_timeout':0}, {'script_timeout':float('nan')}, {'review':True},
                                     {'assignee':'codex'}, {'model':'x'}, {'panel':True}, {'kind':'epic'}])
def test_invalid_script_options(env, tmp_path, kwargs):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        create(conn, tmp_path, **kwargs)


def test_script_refuses_assignment_and_steer(env, tmp_path):
    _, conn, _ = env
    cid = create(conn, tmp_path)
    with pytest.raises(api.CardError):
        api.assign(conn, cid, 'codex', executors=['codex'])
    with pytest.raises(api.CardError):
        api.steer(conn, cid, 'change', 'secretary')


def test_reaped_script_does_not_consult_model_budget_or_retry(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    cid = create(conn, tmp_path)
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    monkeypatch.setattr('hive.heads.weekly_left', lambda *a: pytest.fail('script requested model budget'))
    d.tick()
    r = api.get(conn, cid)['runs'][0]
    d.clock = lambda: r['started_at'] + cfg['spawn_grace_s'] + 1
    d.tick()
    c = api.get(conn, cid)
    assert c['state'] == 'blocked' and len(c['runs']) == 1
    assert c['runs'][0]['provider_failure'] is None


def deferring(env, tmp_path, monkeypatch):
    """A script that writes to stderr and exits with the code in tmp_path/code, launched synchronously on a fake clock."""
    cfg, conn, d = env
    cfg.update(script_defer_s=900, script_defer_max=2)
    now = fake_time(monkeypatch, d)
    monkeypatch.setattr(d, 'spawn', lambda *a: None)
    (tmp_path / 'code').write_text('75')
    cid = create(conn, tmp_path, f'printf "pressure, retry later" >&2; exit $(cat {tmp_path / "code"})')
    return cfg, conn, d, now, cid


def run_once(cfg, conn, d, cid):
    """Tick one launch, run it in-process and tick its result; return the run id."""
    d.tick()
    run = api.get(conn, cid)['runs'][-1]
    assert run['status'] == 'starting'
    runner.main(['--db', cfg['db'], '--run-id', str(run['id'])])
    d.tick()
    return run['id']


def deferred(conn, cid):
    return [e['data'] for e in api.get(conn, cid)['events'] if e['kind'] == 'deferred']


def test_exit_75_requeues_after_defer_delay_without_failure(env, tmp_path, monkeypatch):
    cfg, conn, d, now, cid = deferring(env, tmp_path, monkeypatch)
    rid = run_once(cfg, conn, d, cid)
    c = api.get(conn, cid)
    assert (c['state'], c['retry_at']) == ('queued', 1900.0)
    assert deferred(conn, cid) == [{'run': rid, 'round': 1, 'retry_at': 1900.0, 'deferrals': 1,
                                    'stderr': 'pressure, retry later'}]
    assert not [e for e in c['events'] if e['kind'] in ('retry', 'blocked')]

    now[0] = 1899.9
    d.tick()
    assert len(api.get(conn, cid)['runs']) == 1

    now[0] = 1900.0
    (tmp_path / 'code').write_text('0')
    run_once(cfg, conn, d, cid)
    c = api.get(conn, cid)
    assert (c['state'], len(c['runs'])) == ('done', 2)


def test_exit_75_past_defer_max_blocks_and_other_exit_still_blocks(env, tmp_path, monkeypatch):
    cfg, conn, d, now, cid = deferring(env, tmp_path, monkeypatch)
    for _ in range(2):
        run_once(cfg, conn, d, cid)
        now[0] = api.get(conn, cid)['retry_at']
    run_once(cfg, conn, d, cid)
    c = api.get(conn, cid)
    assert (c['state'], len(deferred(conn, cid))) == ('blocked', 2)
    assert c['block_reason'] == 'script exit=75 after 2 deferrals'


def test_exit_1_after_a_deferral_blocks(env, tmp_path, monkeypatch):
    cfg, conn, d, now, cid = deferring(env, tmp_path, monkeypatch)
    run_once(cfg, conn, d, cid)
    now[0] = api.get(conn, cid)['retry_at']
    (tmp_path / 'code').write_text('1')
    run_once(cfg, conn, d, cid)
    c = api.get(conn, cid)
    assert (c['state'], len(deferred(conn, cid))) == ('blocked', 1)
    assert 'exit=1' in c['block_reason']
