"""Occurrence-bound admission with synthetic clocks and an intercepted script launcher."""
import json
import sqlite3
from pathlib import Path

import pytest
from test_hive import env, hive_cli  # noqa: F401
from test_cron_schedule import stamp
from hive import api, dispatcher, heads, store


@pytest.fixture
def night(env, tmp_path, monkeypatch):
    cfg, conn, d = env
    clock = [stamp('2026-11-09T02:00:00+02:00')]
    monkeypatch.setattr(api.time, 'time', lambda: clock[0])
    d.clock = lambda: clock[0]
    script = tmp_path / 'night.sh'
    script.write_text('exit 0')
    spawns = []
    monkeypatch.setattr(d, 'spawn', lambda *args: spawns.append(args))
    room = [False]
    monkeypatch.setattr(d, 'resource_room', lambda urgent=False: room[0])

    def create(**kwargs):
        options = dict(kind='recurring', cron='0 3 * * *', tz='Europe/Athens', misfire='skip',
                       start_window='03:00-05:00', script=str(script), script_timeout=900, deliver='local')
        options.update(kwargs)
        return api.create(conn, title='night', goal='check', acceptance='true', step=1,
                          source='system:test', created_by='secretary', **options)[0]

    return cfg, conn, d, clock, room, spawns, create


def materialize(night):
    _, conn, d, clock, _, _, create = night
    sid = create()
    clock[0] = stamp('2026-11-09T03:00:00+02:00')
    d.tick()
    return sid, api.get(conn, sid)['instances'][0]['id']


@pytest.mark.parametrize('when,state', [
    ('2026-11-09T02:59:59+02:00', 'queued'),
    ('2026-11-09T03:00:00+02:00', 'running'),
    ('2026-11-09T04:59:59.999+02:00', 'running'),
    ('2026-11-09T05:00:00+02:00', 'cancelled'),
    ('2026-11-09T10:48:00+02:00', 'cancelled'),
    ('2026-11-10T03:00:00+02:00', 'cancelled'),
])
def test_admission_uses_original_absolute_bounds(night, when, state):
    _, conn, d, clock, room, spawns, _ = night
    sid, cid = materialize(night)
    child = api.get(conn, cid)
    assert (child['series_start_window'], child['series_tz']) == ('03:00-05:00', 'Europe/Athens')
    assert (child['eligible_from'], child['eligible_until']) == (
        stamp('2026-11-09T03:00:00+02:00'), stamp('2026-11-09T05:00:00+02:00'))
    clock[0], room[0] = stamp(when), True
    d.launch()
    d.launch()
    child = api.get(conn, cid)
    assert child['state'] == state
    assert len(child['runs']) == len(spawns) == (1 if state == 'running' else 0)
    parent = api.get(conn, sid)
    assert parent['schedule']['next_start'] == stamp('2026-11-10T03:00:00+02:00')
    assert parent['schedule']['stopped'] is False
    if state == 'cancelled':
        assert 'start window expired' in child['result']
        assert [e['kind'] for e in child['events']].count('cancelled') == 1
        assert json.loads(child['outbox'][0]['payload'])['reason'] == child['result']


@pytest.mark.parametrize('pool_full', [False, True])
def test_expiry_does_not_wait_for_resources_or_pool(night, pool_full):
    cfg, conn, d, clock, room, spawns, _ = night
    sid, cid = materialize(night)
    if pool_full:
        cfg['script_concurrency'] = 0
        room[0] = True
    clock[0] = stamp('2026-11-09T05:00:00+02:00')
    d.tick()
    room[0], cfg['script_concurrency'] = True, 4
    d.tick()
    assert api.get(conn, cid)['state'] == 'cancelled'
    assert api.get(conn, cid)['runs'] == [] and spawns == []
    clock[0] = stamp('2026-11-10T03:00:00+02:00')
    d.tick()
    parent = api.get(conn, sid)
    assert [i['state'] for i in parent['instances']] == ['cancelled', 'running']
    assert len(spawns) == 1
    assert parent['schedule']['next_start'] == stamp('2026-11-11T03:00:00+02:00')


def test_deadline_is_sampled_after_workspace_preparation(night, monkeypatch):
    _, conn, d, clock, room, spawns, _ = night
    _, cid = materialize(night)
    clock[0], room[0] = stamp('2026-11-09T04:59:59+02:00'), True
    prepare = heads.prepare_workspace

    def delayed_prepare(*args):
        path = prepare(*args)
        clock[0] = stamp('2026-11-09T05:00:00+02:00')
        return path

    monkeypatch.setattr(heads, 'prepare_workspace', delayed_prepare)
    d.tick()
    child = api.get(conn, cid)
    assert child['state'] == 'cancelled' and child['runs'] == [] and spawns == []


def test_occurrence_before_window_waits_until_inclusive_start(night):
    _, conn, d, clock, room, spawns, create = night
    clock[0] = stamp('2026-11-09T01:00:00+02:00')
    sid = create(cron='0 2 * * *')
    clock[0], room[0] = stamp('2026-11-09T02:00:00+02:00'), True
    d.tick()
    cid = api.get(conn, sid)['instances'][0]['id']
    assert api.get(conn, cid)['state'] == 'queued' and spawns == []
    clock[0] = stamp('2026-11-09T03:00:00+02:00')
    d.tick()
    assert api.get(conn, cid)['state'] == 'running' and len(spawns) == 1


def test_restart_preserves_expiry_and_cas_claim(night, monkeypatch):
    cfg, conn, d, clock, room, spawns, _ = night
    sid, cid = materialize(night)
    bounds = api.get(conn, cid)['eligible_until']
    other = store.connect(cfg['db'])
    try:
        restarted = dispatcher.Dispatcher(cfg, other, clock=lambda: clock[0])
        monkeypatch.setattr(restarted, 'spawn', lambda *args: spawns.append(args))
        monkeypatch.setattr(restarted, 'resource_room', lambda urgent=False: True)
        clock[0] = stamp('2026-11-09T10:48:00+02:00')
        restarted.tick()
        assert api.get(other, cid)['eligible_until'] == bounds
        assert api.get(other, cid)['state'] == 'cancelled' and spawns == []
        clock[0] = stamp('2026-11-10T03:00:00+02:00')
        restarted.advance()
        next_id = api.get(other, sid)['instances'][1]['id']
        stale = api.card_row(conn, next_id)
        restarted.launch()
        room[0] = True
        assert d.start_script(stale) is None
        assert len(api.get(conn, next_id)['runs']) == len(spawns) == 1
        assert api.get(conn, next_id)['eligible_until'] == stamp('2026-11-10T05:00:00+02:00')
    finally:
        other.close()


@pytest.mark.parametrize('released', ['04:30:00', '05:00:00'])
def test_dependency_delay_retains_deadline(night, released):
    _, conn, d, clock, room, spawns, create = night
    sid, cid = materialize(night)
    blocker = create(kind='task', cron=None, tz=None, misfire=None, start_window=None)
    api.add_blocker(conn, cid, blocker)
    assert api.get(conn, cid)['wait_reason'] == api.WAIT_DEPENDENCY
    clock[0] = stamp(f'2026-11-09T{released}+02:00')
    if released == '05:00:00':
        d.tick()
        assert api.get(conn, cid)['state'] == 'cancelled'
    room[0] = True
    d.tick()
    run = api.get(conn, blocker)['runs'][0]
    conn.execute("UPDATE runs SET status='exited',exit_code=0,report='completed' WHERE id=?", (run['id'],))
    d.tick()
    d.tick()
    child = api.get(conn, cid)
    assert api.get(conn, blocker)['state'] == 'done'
    assert child['state'] == ('running' if released == '04:30:00' else 'cancelled')
    assert len(child['runs']) == (1 if released == '04:30:00' else 0)
    assert len(spawns) == (2 if released == '04:30:00' else 1)
    assert api.get(conn, sid)['schedule']['next_start'] == stamp('2026-11-10T03:00:00+02:00')


@pytest.mark.parametrize('overlap', ['skip', 'queue'])
def test_stale_child_cannot_block_next_night(night, overlap):
    _, conn, d, clock, room, spawns, create = night
    sid = create(overlap=overlap)
    clock[0] = stamp('2026-11-09T03:00:00+02:00')
    d.tick()
    old = api.get(conn, sid)['instances'][0]['id']
    clock[0], room[0] = stamp('2026-11-10T03:00:00+02:00'), True
    d.tick()
    parent = api.get(conn, sid)
    assert [i['state'] for i in parent['instances']] == ['cancelled', 'running']
    assert api.get(conn, old)['runs'] == [] and len(spawns) == 1
    assert parent['schedule']['skipped'] == 0


def test_missed_materialization_skips_daytime_catchup(night):
    _, conn, d, clock, room, spawns, create = night
    sid = create()
    clock[0], room[0] = stamp('2026-11-09T10:48:00+02:00'), True
    d.tick()
    card = api.get(conn, sid)
    assert card['instances'] == [] and spawns == []
    assert card['schedule']['skipped'] == 1 and card['schedule']['misfire'] == 'skip'
    assert card['schedule']['next_start'] == stamp('2026-11-10T03:00:00+02:00')


@pytest.mark.parametrize('before,due,end,inside', [
    ('2026-03-28T23:00:00+02:00', '2026-03-29T04:00:00+03:00',
     '2026-03-29T05:00:00+03:00', '2026-03-29T04:30:00+03:00'),
    ('2026-10-24T23:00:00+03:00', '2026-10-25T03:00:00+03:00',
     '2026-10-25T05:00:00+02:00', '2026-10-25T03:30:00+02:00'),
])
def test_dst_bounds_are_bound_to_occurrence_date(night, before, due, end, inside):
    _, conn, d, clock, room, spawns, create = night
    clock[0] = stamp(before)
    sid = create()
    assert api.get(conn, sid)['schedule']['next_start'] == stamp(due)
    clock[0] = stamp(due)
    d.tick()
    cid = api.get(conn, sid)['instances'][0]['id']
    child = api.get(conn, cid)
    assert (child['eligible_from'], child['eligible_until']) == (stamp(due), stamp(end))
    clock[0], room[0] = stamp(inside), True
    d.tick()
    assert api.get(conn, cid)['state'] == 'running' and len(spawns) == 1


def test_admitted_run_finishes_after_deadline_under_existing_timeout(night, monkeypatch):
    _, conn, d, clock, room, spawns, _ = night
    _, cid = materialize(night)
    clock[0], room[0] = stamp('2026-11-09T04:59:59+02:00'), True
    d.tick()
    run = api.get(conn, cid)['runs'][0]
    job = json.loads((Path(run['run_dir']) / 'job.json').read_text())
    assert job['timeout'] == 900
    monkeypatch.setattr(d, 'kill', lambda *args: pytest.fail('deadline killed an active run'))
    monkeypatch.setattr(d, 'alive', lambda *args: True)
    conn.execute("UPDATE runs SET status='running',heartbeat_at=? WHERE id=?", (clock[0], run['id']))
    clock[0] = stamp('2026-11-09T05:01:00+02:00')
    d.tick()
    assert api.get(conn, cid)['state'] == 'running'
    conn.execute("UPDATE runs SET status='exited',exit_code=0,report='completed' WHERE id=?", (run['id'],))
    d.tick()
    child = api.get(conn, cid)
    assert (child['state'], child['result']) == ('done', 'completed')
    assert len(child['runs']) == len(spawns) == 1


def test_jobs_without_window_keep_delayed_admission(night):
    _, conn, d, clock, room, spawns, create = night
    sid = create(start_window=None)
    clock[0] = stamp('2026-11-09T03:00:00+02:00')
    d.tick()
    clock[0], room[0] = stamp('2026-11-09T10:48:00+02:00'), True
    d.tick()
    child = api.get(conn, api.get(conn, sid)['instances'][0]['id'])
    assert child['state'] == 'running' and len(spawns) == 1


def test_missing_occurrence_bounds_fail_closed(night):
    _, conn, d, _, room, spawns, _ = night
    _, cid = materialize(night)
    conn.execute('UPDATE cards SET eligible_until=NULL WHERE id=?', (cid,))
    room[0] = True
    d.tick()
    child = api.get(conn, cid)
    assert child['state'] == 'cancelled' and child['runs'] == [] and spawns == []
    assert child['result'] == 'start window refused: missing occurrence bounds'


@pytest.mark.parametrize('options', [
    {'start_window': '3:00-05:00'}, {'start_window': '03:00-24:00'},
    {'start_window': '03:60-05:00'}, {'start_window': '05:00-03:00'},
    {'start_window': '03:00-03:00'}, {'start_window': '03:00:00-05:00:00'},
    {'start_window': []}, {'tz': 'No/Zone'}, {'tz': ''},
    {'misfire': 'latest'}, {'script': None},
    {'cron': None, 'every': 60, 'max_runs': 2},
    {'kind': 'task', 'cron': None}, {'executor': 'dots'}, {'model': 'gpt-6.1-sol'},
])
def test_invalid_window_configuration_is_rejected(night, options):
    *_, create = night
    with pytest.raises(api.CardError):
        create(**options)


def test_cli_configuration_and_inspection(night, monkeypatch, capsys):
    cfg, _, _, _, _, _, _ = night
    script = Path(cfg['db']).parent / 'night.sh'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'night', '--goal', 'check', '--step', '1',
                    '--source', 'system:test', '--created-by', 'secretary', '--cron', '0 3 * * *',
                    '--tz', 'Europe/Athens', '--misfire', 'skip', '--start-window', '03:00-05:00',
                    '--script', str(script), '--json') == 0
    result = json.loads(capsys.readouterr().out)
    cid = result['id']
    assert hive_cli(monkeypatch, cfg, 'show', cid, '--json') == 0
    card = json.loads(capsys.readouterr().out)
    assert card['schedule']['start_window'] == '03:00-05:00'
    assert card['schedule']['next_start'] == stamp('2026-11-09T03:00:00+02:00')
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    output = capsys.readouterr().out
    assert '03:00-05:00 (Europe/Athens)' in output and 'misfire: skip' in output


def test_additive_migration_keeps_legacy_jobs_unrestricted(tmp_path):
    path = tmp_path / 'legacy.db'
    old = sqlite3.connect(path)
    old.executescript(store.SCHEMA + store.RUNS_TABLE)
    old.execute("""INSERT INTO cards(id,title,goal,acceptance,step,source,created_by,state,created_at,updated_at)
                   VALUES('legacy','legacy','check','true',1,'system:test','secretary','queued',1,1)""")
    old.commit()
    old.close()
    migrated = store.connect(path, owner_actor='owner')
    try:
        child = api.get(migrated, 'legacy')
        assert child['state'] == 'queued'
        assert child['series_start_window'] is None
        assert child['eligible_from'] is None and child['eligible_until'] is None
    finally:
        migrated.close()
