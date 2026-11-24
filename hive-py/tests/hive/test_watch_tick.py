"""Watch tick and child acceptance against a temporary Hive DB, fake collectors and no network."""
import json
import time
from pathlib import Path

import pytest
from test_hive import env  # noqa: F401
from hive import api, heads, store, watch
from hive import watch_gate as wg

JOB = 'job1'
T0 = 1_900_000_000.0
COLLECTOR = '''import pathlib, sys, time
state = pathlib.Path(sys.argv[1])
mode = (state / 'mode').read_text().strip() if (state / 'mode').exists() else 'ok'
print('collector saw ' + __import__('os').environ.get('GH_TOKEN', 'nothing'), file=sys.stderr)
if mode == 'exit':
    sys.exit(3)
if mode == 'sleep':
    time.sleep(30)
print('not json' if mode == 'garbage' else (state / 'signals.json').read_text())
'''


class World:
    def __init__(self, cfg, conn, d, tmp_path):
        self.cfg, self.conn, self.d, self.tmp = cfg, conn, d, tmp_path
        self.state = tmp_path / 'collector-state'
        self.state.mkdir()
        (tmp_path / 'collector.py').write_text(COLLECTOR)
        hold = time.time() + 10 ** 8
        self.series = api.create(conn, title='watch series', goal='tick', acceptance='true', step=1,
                                 source=f'cron:{JOB}', created_by=f'cron:{JOB}', kind='recurring', every=1800,
                                 max_runs=1, not_before=hold, deliver='local')[0]
        self.tick_card = api.create(conn, title='watch tick', goal='tick', acceptance='true', step=1,
                                    source=f'cron:{JOB}', created_by=f'cron:{JOB}', not_before=hold, deliver='local')[0]
        with store.tx(conn):
            conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)",
                         (self.tick_card, self.series, time.time()))
        self.workdir = tmp_path / 'tick'
        self.workdir.mkdir()
        self.now = T0

    def job(self, news_destination='owner-inbox', timeout=10, lease=4000, **policy):
        path = self.tmp / 'watch.json'
        path.write_text(json.dumps({
            'job': JOB, 'prefixes': ['upstream:'], 'checkpoint': str(self.tmp / 'checkpoint.json'),
            'collector': {'script': str(self.tmp / 'collector.py'), 'args': [str(self.state)], 'timeout': timeout},
            'policy': {'news_destination': news_destination, 'lease': lease, **policy},
            'child': {'step': 1, 'title': 'Upstream issue watch', 'prompt': 'Report substantive issue news.'}}))
        return watch.load(path, self.cfg['owner_tz'])

    def emit(self, *signals, mode='ok'):
        (self.state / 'mode').write_text(mode)
        (self.state / 'signals.json').write_text(json.dumps({'signals': [
            {'key': k, 'class': 'news', 'fingerprint': f} for k, f in signals]}))

    def emit_with_errors(self, *signals, errors=()):
        (self.state / 'mode').write_text('ok')
        (self.state / 'signals.json').write_text(json.dumps({'signals': [
            {'key': k, 'class': 'news', 'fingerprint': f} for k, f in signals] + [
            {'key': k, 'class': 'source_error', 'fingerprint': f} for k, f in errors]}))

    def start(self, job, *signals):
        with wg.locked(job.checkpoint, wg.baseline([wg.Signal(k, 'news', f) for k, f in signals], self.now)):
            pass
        self.emit(*signals)

    def tick(self, job, step=60):
        self.now += step
        return watch.tick(job, self.cfg, self.conn, self.tick_card, self.workdir, now=self.now)

    def children(self, job):
        return [dict(r) for r in self.conn.execute('SELECT * FROM cards WHERE source=? ORDER BY created_at, rowid',
                                                   (job.source,))]

    def live_children(self, job):
        return [c for c in self.children(job) if c['state'] != 'cancelled']

    def handed(self, child):
        data = json.loads((Path(self.cfg['workspaces']) / child['id'] / 'input.json').read_text())
        return [(i['key'], i['change'], i['class']) for i in data['handoff']['items']]

    def finish_worker(self, child, status='exited', **result):
        """Record a finished worker run with its result files, as the runner would, then run one dispatcher tick."""
        workdir = heads.prepare_workspace(self.cfg, child)
        if result:
            message = result.setdefault('message', 'Issue reopened by maintainer')
            result = {'job_id': JOB, 'status': 'ok', 'news': True, 'evidence': ['read input.json'], **result}
            (workdir / 'result.json').write_text(json.dumps(result))
            (workdir / 'REPORT.md').write_text(message)
            (workdir / 'EVIDENCE.md').write_text('checked the handoff')
        with store.tx(self.conn):
            card = api.card_row(self.conn, child['id'])
            assert store.cas_card(self.conn, card, 'running', round=1, workdir=str(workdir))
            rid = self.conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,
                                       started_at,ended_at,exit_code,report,run_dir)
                                       VALUES(?,1,'worker','claude','m','e',1,'',?,?,?,?,?,?)""",
                                    (child['id'], status, time.time(), time.time(), 0 if status == 'exited' else None,
                                     result.get('message', ''), str(workdir))).lastrowid
            store.event(self.conn, child['id'], 'claimed', {'run': rid, 'role': 'worker'})
        if status == 'exited':
            self.d.tick()
        return api.card_row(self.conn, child['id'])


@pytest.fixture
def world(env, tmp_path, monkeypatch):  # noqa: F811
    cfg, conn, d = env
    profile = tmp_path / 'profile'
    profile.mkdir()
    (profile / '.env').write_text('export GH_TOKEN="ghp-synthetic-secret"\n')
    cfg['script_env_file'] = str(profile / '.env')
    monkeypatch.setattr(d, 'launch', lambda: [])
    return World(cfg, conn, d, tmp_path)


def outbox(conn, child):
    return conn.execute("SELECT * FROM outbox WHERE card_id=? AND kind='task_result'", (child['id'],)).fetchone()


def test_unchanged_signals_over_many_ticks_create_no_child(world):
    job = world.job()
    world.start(job, ('upstream:issue', 'open'), ('upstream:comment:1', 'first'))
    summaries = [world.tick(job, step=1800) for _ in range(20)]
    assert {s['action'] for s in summaries} == {'silent'}
    assert world.children(job) == []


def test_change_creates_one_owner_inbox_child_with_link_source_and_rendered_diff(world):
    job = world.job()
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'closed'))
    summary = world.tick(job)
    assert summary['action'] == 'wake'
    for _ in range(3):
        assert world.tick(job)['action'] == 'wait-active'
    [child] = world.children(job)
    assert (child['source'], child['deliver'], child['state'], child['not_before']) == (
        f'cron:{JOB}:conditional', 'owner-inbox', 'created', None)
    assert child['idem_key'] == f"watch:{JOB}:{summary['gate']}"
    link = world.conn.execute("SELECT target_id FROM links WHERE card_id=? AND kind='cron-agent'",
                              (child['id'],)).fetchone()
    assert link['target_id'] == world.series
    data = json.loads((Path(world.cfg['workspaces']) / child['id'] / 'input.json').read_text())
    assert data['gate'] == summary['gate']
    assert 'changed news upstream:issue: closed (was: open)' in data['script_output']
    assert world.handed(child) == [('upstream:issue', 'changed', 'news')]


def test_tick_interrupted_before_release_resumes_the_same_child(world, monkeypatch):
    job = world.job()
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'closed'))
    real = heads.prepare_workspace
    monkeypatch.setattr(heads, 'prepare_workspace', lambda *a: (_ for _ in ()).throw(OSError('disk full')))
    with pytest.raises(OSError):
        world.tick(job)
    [held] = world.children(job)
    assert held['not_before'] is not None
    monkeypatch.setattr(heads, 'prepare_workspace', real)
    assert world.tick(job)['settled']['outcome'] == 'active'
    [child] = world.children(job)
    assert child['id'] == held['id'] and child['not_before'] is None
    assert world.handed(child) == [('upstream:issue', 'changed', 'news')]


@pytest.mark.parametrize('mode,timeout,fingerprint', [
    ('exit', 10, 'collector exited with status 3'),
    ('sleep', 0.5, 'collector timed out after 0.5s'),
    ('garbage', 10, 'collector output is not a valid signal list')])
def test_collector_failure_wakes_an_owner_inbox_source_error_child(world, mode, timeout, fingerprint):
    job = world.job(news_destination='telegram', timeout=timeout)
    world.start(job, ('upstream:issue', 'open'))
    world.emit(mode=mode)
    assert world.tick(job)['action'] == 'wake'
    [child] = world.children(job)
    assert child['deliver'] == 'owner-inbox'
    assert world.handed(child) == [('src:collector', 'new', 'source_error')]
    assert fingerprint in json.loads((Path(world.cfg['workspaces']) / child['id'] / 'input.json').read_text())[
        'script_output']
    log = (world.workdir / 'collector.stderr.log').read_text()
    assert 'collector saw [REDACTED]' in log and 'ghp-synthetic-secret' not in log


def test_source_error_only_handoff_never_becomes_telegram(world):
    job = world.job(news_destination='telegram')
    world.start(job, ('upstream:issue', 'open'))
    world.emit(mode='exit')
    world.tick(job)
    [child] = world.children(job)
    child = world.finish_worker(child, message='Collector is failing', news=True)
    assert (child['state'], child['deliver']) == ('done', 'owner-inbox')
    assert outbox(world.conn, child)['published_at'] is not None
    assert world.tick(job)['settled']['outcome'] == 'delivered'


def test_substantive_news_goes_to_inbox_and_acks_only_after_publication(world, monkeypatch):
    job = world.job(news_destination='telegram')
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'open'), ('upstream:comment:9', 'maintainer asks for logs'))
    world.tick(job)
    [child] = world.children(job)

    original = world.d.publish_inbox
    def down(*args, **kw):
        raise RuntimeError('inbox unavailable')
    monkeypatch.setattr(world.d, 'publish_inbox', down)
    child = world.finish_worker(child, message='Maintainer asks for logs', news=True)
    assert (child['state'], child['deliver']) == ('done', 'telegram')
    assert outbox(world.conn, child)['published_at'] is None
    for _ in range(3):
        summary = world.tick(job)
        assert (summary['settled']['outcome'], summary['action']) == ('delivering', 'wait-active')
    monkeypatch.setattr(world.d, 'publish_inbox', original)
    world.d.publish()
    event_id = outbox(world.conn, child)['event_id']
    assert 'Maintainer asks for logs' in world.d.inbox().packets[event_id]['payload']
    assert world.tick(job)['settled']['outcome'] == 'delivered'
    assert wg.read(job.checkpoint).pending is None
    assert world.tick(job)['action'] == 'silent'
    assert len(world.children(job)) == 1


def test_steer_before_check_keeps_substantive_news_in_owner_inbox(world):
    job = world.job(news_destination='telegram')
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'open'), ('upstream:comment:9', 'maintainer asks for logs'))
    world.tick(job)
    [child] = world.children(job)
    api.steer(world.conn, child['id'], 'mention the issue number', 'owner')
    child = world.finish_worker(child, message='Maintainer asks for logs', news=True)
    assert (child['state'], child['deliver']) == ('done', 'owner-inbox')
    assert world.tick(job)['settled']['outcome'] == 'delivered'


def test_late_steer_after_telegram_check_never_publishes_a_failure_to_telegram(world, monkeypatch):
    job = world.job(news_destination='telegram')
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'open'), ('upstream:comment:9', 'maintainer asks for logs'))
    world.tick(job)
    [first] = world.children(job)
    real = world.d.acceptance

    def steered(card):
        result = real(card)
        api.steer(world.conn, card['id'], 'also check the linked PR', 'owner')
        return result
    monkeypatch.setattr(world.d, 'acceptance', steered)
    reopened = world.finish_worker(first, message='Maintainer asks for logs', news=True)
    assert (reopened['state'], reopened['deliver']) == ('queued', 'telegram')
    monkeypatch.setattr(world.d, 'acceptance', real)
    world.finish_worker(reopened, status='crashed')
    world.d.tick()
    summary = world.tick(job)
    world.d.publish()
    assert summary['settled']['outcome'] == 'abandoned'
    first = api.card_row(world.conn, first['id'])
    assert (first['state'], first['deliver']) == ('cancelled', 'owner-inbox')
    assert world.conn.execute("SELECT published_at FROM outbox WHERE card_id=? AND kind='task_result'",
                              (first['id'],)).fetchone()['published_at'] is not None
    [second] = world.live_children(job)
    assert world.handed(second) == [('upstream:comment:9', 'new', 'news')]


def test_silent_result_ends_local_and_acks(world):
    job = world.job(news_destination='telegram')
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'open'), ('upstream:comment:9', 'thanks'))
    world.tick(job)
    [child] = world.children(job)
    child = world.finish_worker(child, message='[SILENT]', news=False)
    assert (child['state'], child['deliver']) == ('done', 'local')
    assert world.tick(job)['settled']['outcome'] == 'delivered'
    assert world.tick(job)['action'] == 'silent'


def crash(world, child):
    world.finish_worker(child, status='crashed')


def time_out(world, child):
    world.finish_worker(child, status='timeout')


def controller_cancel(world, child):
    world.d.cancel(child['id'], 'cron migration: failed real attempt', 'secretary')


def acceptance_failure(world, child):
    child = world.finish_worker(child, status='exited', message='half done', news=True, evidence=[])
    assert child['state'] == 'queued' and child['deliver'] == 'owner-inbox'


def lease_expiry(world, child):
    world.now += 4000


@pytest.mark.parametrize('failure', [crash, time_out, controller_cancel, acceptance_failure, lease_expiry])
def test_failed_child_leaves_items_owed_and_next_tick_retries_once(world, failure):
    job = world.job()
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'closed'))
    world.tick(job)
    [first] = world.children(job)
    failure(world, first)
    summary = world.tick(job)
    assert summary['settled']['outcome'] in ('abandoned', 'failed', 'starved') and summary['action'] == 'wake'
    assert api.card_row(world.conn, first['id'])['state'] == 'cancelled'
    if failure is lease_expiry:
        assert summary['settled']['outcome'] == 'starved'
        assert 'not started / starved' in api.card_row(world.conn, first['id'])['result']
    [second] = world.live_children(job)
    assert second['id'] != first['id']
    assert world.handed(second) == [('upstream:issue', 'changed', 'news')]
    assert world.tick(job)['action'] == 'wait-active'
    assert len(world.live_children(job)) == 1


def test_child_done_between_expiry_and_cancel_settles_instead_of_failing_tick(world, monkeypatch):
    """The lease expires while the finished worker waits for its gate; the gate passes just before the tick's
    cancel, which must settle the handoff instead of raising CardError (already final) out of the tick."""
    from hive.dispatcher import Dispatcher
    job = world.job()
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'closed'))
    world.tick(job)
    [first] = world.children(job)
    gate = type(world.d).tick
    monkeypatch.setattr(world.d, 'tick', lambda: None)
    world.finish_worker(first, message='[SILENT]', news=False)
    assert api.card_row(world.conn, first['id'])['state'] == 'running'
    real_cancel = Dispatcher.cancel

    def racing(self, card_id, *a, **kw):
        gate(world.d)
        assert api.card_row(world.conn, card_id)['state'] == 'done'
        return real_cancel(self, card_id, *a, **kw)
    monkeypatch.setattr(Dispatcher, 'cancel', racing)
    world.now += 4000
    summary = world.tick(job)
    assert summary['settled']['outcome'] in ('delivered', 'delivering')
    assert api.card_row(world.conn, first['id'])['state'] == 'done'
    monkeypatch.setattr(Dispatcher, 'cancel', real_cancel)
    world.d.publish()
    if summary['settled']['outcome'] == 'delivering':
        assert world.tick(job)['settled']['outcome'] == 'delivered'
    assert world.tick(job)['action'] == 'silent'
    assert [c['id'] for c in world.children(job)] == [first['id']]


def test_signal_vanished_during_in_flight_child_is_handed_on_next_wake(world):
    job = world.job()
    world.start(job, ('upstream:issue', 'open'))
    world.emit(('upstream:issue', 'closed'))
    world.tick(job)
    [first] = world.children(job)
    world.emit(('upstream:issue', 'closed'), ('upstream:comment:5', 'short-lived'))
    assert world.tick(job)['action'] == 'wait-active'
    world.emit(('upstream:issue', 'closed'))
    assert world.tick(job)['action'] == 'wait-active'
    world.finish_worker(first, message='Issue closed', news=True)
    summary = world.tick(job)
    assert (summary['settled']['outcome'], summary['action']) == ('delivered', 'wake')
    second = world.live_children(job)[-1]
    assert world.handed(second) == [('upstream:comment:5', 'new', 'news')]


def test_failed_tick_switches_its_own_card_to_owner_inbox(world, monkeypatch):
    job = world.job()
    with store.tx(world.conn):
        card = api.card_row(world.conn, world.tick_card)
        store.cas_card(world.conn, card, 'running', round=1)
    monkeypatch.setenv('HIVE_CARD', world.tick_card)
    monkeypatch.chdir(world.workdir)
    with pytest.raises(wg.CheckpointMissing):
        watch.main(['tick', str(job.path)])
    assert api.card_row(world.conn, world.tick_card)['deliver'] == 'owner-inbox'


def test_lease_must_outlast_child_run_and_acceptance(world):
    job = world.job(lease=1500)
    world.start(job, ('upstream:issue', 'open'))
    with pytest.raises(watch.ConfigError, match='lease'):
        world.tick(job)


def test_quiet_policy_cleared_only_and_flapping_error_ticks_start_no_child(world):
    job = world.job(wake_on_cleared=False, error_grace=7200)
    world.start(job, ('upstream:one', 'open'), ('upstream:two', 'open'))
    world.emit_with_errors(('upstream:one', 'open'))
    assert world.tick(job, step=1800)['action'] == 'silent'
    assert set(wg.read(job.checkpoint).baseline) == {'upstream:one'}
    for n in range(6):
        world.emit_with_errors(('upstream:one', 'open'), errors=[('src:upstream', 'invalid output: ValueError')] * (n % 2 == 0))
        assert world.tick(job, step=1800)['action'] == 'silent'
    assert world.children(job) == []
    assert wg.read(job.checkpoint).suspects == {}


def test_quiet_policy_still_wakes_for_new_news(world):
    job = world.job(wake_on_cleared=False, error_grace=3000)
    world.start(job, ('upstream:one', 'open'))
    world.emit_with_errors(('upstream:one', 'open'), ('upstream:two', 'new'))
    summary = world.tick(job, step=1800)
    assert summary['action'] == 'wake' and len(world.children(job)) == 1
    assert [i[1] for i in world.handed(world.children(job)[0])] == ['new']
