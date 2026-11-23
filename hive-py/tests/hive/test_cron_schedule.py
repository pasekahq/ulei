"""Cron timezone/DST, cursor atomicity, downtime, bounds and legacy intervals."""
from datetime import datetime
from zoneinfo import ZoneInfo

import pytest
from test_hive import env  # noqa: F401
from hive import api, store, dispatcher
from hive.schedule import next_cron


def stamp(value):
    return datetime.fromisoformat(value).timestamp()


def series(conn, **kwargs):
    return api.create(conn, title='cron', goal='check', acceptance='true', step=1,
                      source='system:test', created_by='secretary', kind='recurring', cron='0 8 * * *', **kwargs)[0]


def test_default_timezone_and_unbounded_series(env, monkeypatch):
    cfg, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn)
    card = api.get(conn, sid)
    assert card['schedule']['tz'] == cfg['owner_tz'] == 'Europe/Athens'
    assert card['schedule']['next_start'] == stamp('2026-11-09T06:00:00Z')
    d.advance_series(stamp('2026-11-09T06:00:10Z'))
    assert len(api.get(conn, sid)['instances']) == 1
    assert api.get(conn, sid)['schedule']['next_start'] == stamp('2026-11-10T06:00:00Z')


def test_dst_fixed_time_spring_and_fall():
    z = ZoneInfo('Europe/Athens')
    spring = next_cron('30 3 * * *', z.key, stamp('2026-03-28T03:30:00+02:00'))
    assert datetime.fromtimestamp(spring, z).isoformat() == '2026-03-29T04:00:00+03:00'
    fall = next_cron('30 3 * * *', z.key, stamp('2026-10-24T03:30:00+03:00'))
    assert fall == stamp('2026-10-25T03:30:00+03:00')
    assert next_cron('30 3 * * *', z.key, fall) == stamp('2026-10-26T03:30:00+02:00')
    # Restart in the repeated hour must not produce a timestamp before its cursor.
    assert next_cron('30 3 * * *', z.key, stamp('2026-10-25T03:10:00+02:00')) == stamp('2026-10-26T03:30:00+02:00')


def test_dst_frequent_jobs_run_both_folds():
    assert next_cron('*/15 * * * *', 'Europe/Athens', stamp('2026-10-25T03:45:00+03:00')) == stamp('2026-10-25T03:00:00+02:00')


def test_restart_and_second_connection_do_not_duplicate_instance(env, monkeypatch):
    cfg, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn)
    now = stamp('2026-11-09T06:00:10Z')
    d.advance_series(now)
    other = store.connect(cfg['db'])
    try:
        dispatcher.Dispatcher(cfg, other).advance_series(now)
        assert conn.execute("SELECT count(*) FROM cards WHERE idem_key=?", (f'series:{sid}:1',)).fetchone()[0] == 1
        assert api.get(conn, sid)['series_next'] == 2
    finally:
        other.close()


@pytest.mark.parametrize('misfire,expected', [('skip', []), ('latest', [3])])
def test_missed_occurrences_not_replayed(env, monkeypatch, misfire, expected):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn, misfire=misfire)
    # 00:30 local on 12.11: the 11.11 08:00 occurrence can no longer be caught up on its own day.
    d.advance_series(stamp('2026-11-11T22:30:00Z'))
    card = api.get(conn, sid)
    assert [i['period'] for i in card['instances']] == expected
    assert card['schedule']['next_start'] == stamp('2026-11-12T06:00:00Z')
    assert card['schedule']['skipped'] == (3 if misfire == 'skip' else 2)


def test_late_tick_catches_up_same_day(env, monkeypatch):
    """An occurrence that a locked DB delays still runs later on its local day."""
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn, deliver='local')
    d.advance_series(stamp('2026-11-09T06:00:01Z'))          # 09.11 on time
    d.advance_series(stamp('2026-11-10T06:02:56Z'))          # 10.11 tick late by ~3 min (locked DB)
    card = api.get(conn, sid)
    assert [i['period'] for i in card['instances']] == [1, 2]
    assert card['schedule']['skipped'] == 0
    d.advance_series(stamp('2026-11-11T19:30:00Z'))          # 11.11 21:30 local: still the same day, run late
    assert [i['period'] for i in api.get(conn, sid)['instances']] == [1, 2, 3]
    assert not conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='periods_skipped'", (sid,)).fetchone()
    assert not conn.execute('SELECT 1 FROM outbox WHERE card_id=?', (sid,)).fetchone()


def test_uncatchable_occurrence_notifies_secretary(env, monkeypatch):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn, deliver='local')
    d.advance_series(stamp('2026-11-09T22:10:00Z'))          # 00:10 local on 10.11: 09.11 08:00 is lost
    card = api.get(conn, sid)
    assert card['instances'] == [] and card['schedule']['skipped'] == 1
    row = conn.execute('SELECT * FROM outbox WHERE card_id=?', (sid,)).fetchone()
    assert row['event_id'] == f'hive:{sid}:missed:1' and row['kind'] == 'task_blocked'
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T22:10:05Z'))
    assert d.publish() == 1                                       # deliver=local does not swallow a lost run
    assert conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='secretary_event'", (sid,)).fetchone()
    assert d.inbox().get(f'hive:{sid}:missed:1') is not None


@pytest.mark.parametrize('overlap', ['skip', 'queue'])
def test_overlap_and_inheritance(env, monkeypatch, tmp_path, overlap):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    script = tmp_path / 'check.sh'
    script.write_text('exit 0')
    sid = series(conn, overlap=overlap, script=str(script), script_timeout=42, deliver='local')
    d.advance_series(stamp('2026-11-09T06:00:00Z'))
    first_id = store.instances(conn, sid)[0]['id']
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (first_id,))
    d.advance_series(stamp('2026-11-10T06:00:00Z'))
    card = api.get(conn, sid)
    assert len(card['instances']) == (1 if overlap == 'skip' else 2)
    first = api.get(conn, card['instances'][0]['id'])
    assert (first['script'], first['script_timeout'], first['deliver']) == (str(script), 42, 'local')
    if overlap == 'queue':
        second = api.get(conn, card['instances'][1]['id'])
        assert second['blocked_by'][0]['id'] == first['id']
    api.stop(conn, sid)
    d.advance_series(stamp('2026-11-11T06:00:00Z'))
    assert api.get(conn, sid)['schedule']['next_start'] is None


@pytest.mark.parametrize('extra', [{'every':60}, {'tz':'Invalid/Zone'}, {'max_runs':0}, {'misfire':'all'}])
def test_invalid_cron_options(env, extra):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        series(conn, **extra)


@pytest.mark.parametrize('expr', ['bad', '99 * * * *', '0 0 30 2 *', '* * * * * *'])
def test_invalid_expression(env, expr):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        api.create(conn, title='invalid', goal='check', acceptance='true', step=1,
                   source='system:test', created_by='secretary', kind='recurring', cron=expr)


def test_cron_max_runs_until_and_not_before(env, monkeypatch):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn, max_runs=1, not_before=stamp('2026-11-10T06:00:00Z'), until=stamp('2026-11-11T06:00:00Z'))
    d.advance_series(stamp('2026-11-09T06:00:00Z'))
    assert not api.get(conn, sid)['instances']
    d.advance_series(stamp('2026-11-10T06:00:00Z'))
    assert len(api.get(conn, sid)['instances']) == 1
    assert api.get(conn, sid)['schedule']['next_start'] is None


def test_cron_instance_and_cursor_roll_back_together(env, monkeypatch):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: stamp('2026-11-09T05:00:00Z'))
    sid = series(conn)
    original = store.cas_card

    def crash(conn, card, state, **fields):
        if card['id'] == sid and 'series_next' in fields:
            raise RuntimeError('synthetic crash before cursor commit')
        return original(conn, card, state, **fields)

    monkeypatch.setattr(store, 'cas_card', crash)
    with pytest.raises(RuntimeError):
        d.advance_series(stamp('2026-11-09T06:00:00Z'))
    assert api.get(conn, sid)['instances'] == []
    assert api.get(conn, sid)['series_next'] == 1
    monkeypatch.setattr(store, 'cas_card', original)
    d.advance_series(stamp('2026-11-09T06:00:00Z'))
    assert len(api.get(conn, sid)['instances']) == 1
