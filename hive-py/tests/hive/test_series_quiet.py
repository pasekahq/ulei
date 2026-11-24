import json

import pytest
from test_hive import env  # noqa: F401
from test_delivery import finished_card
from hive import store


@pytest.mark.parametrize('report,state,visible,kind,expected', [
    ('RESULT: partial next check', 'done', False, 'series_run', False),
    ('RESULT: done news in prose', 'done', False, 'series_run', False),
    ('RESULT: done nothing\n[NEWS]', 'done', False, 'series_run', False),
    ('RESULT: done [NEWS] new fact', 'done', False, 'series_run', True),
    ('RESULT: done [SERIES-STOPPED]', 'done', False, 'series_run', True),
    ('RESULT: partial', 'blocked', False, 'series_run', True),
    ('RESULT: partial', 'cancelled', False, 'series_run', True),
    ('RESULT: partial', 'done', True, 'series_run', True),
    ('RESULT: partial', 'done', False, 'system', True),
])
def test_series_delivery_contract(env, report, state, visible, kind, expected):
    _, conn, d = env
    cid = finished_card(conn, destination='owner-inbox', stdout=report, state=state)
    conn.execute('UPDATE cards SET card_class=?,owner_visible=? WHERE id=?', (kind, visible, cid))
    with store.tx(conn):
        d.finish(conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone(), state, 'finished')
    assert d.publish() == 1
    assert d.publish() == 0
    row = conn.execute('SELECT * FROM outbox WHERE card_id=?', (cid,)).fetchone()
    assert (d.adapter().inbox.get(row['event_id']) is not None) == expected
    assert row['published_at'] is not None and row['error'] is None
    assert conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()[0] == state
    assert conn.execute('SELECT report FROM runs WHERE card_id=?', (cid,)).fetchone()[0] == report
    assert bool(conn.execute("SELECT 1 FROM events WHERE card_id=? AND kind='secretary_event'", (cid,)).fetchone()) == expected


@pytest.mark.parametrize('exception', ['loop', 'stop', 'legacy'])
def test_publication_exceptions_and_legacy_pending(env, exception):
    _, conn, d = env
    cid = finished_card(conn, destination='owner-inbox', stdout='RESULT: partial')
    conn.execute("UPDATE cards SET card_class='series_run',owner_visible=0 WHERE id=?", (cid,))
    with store.tx(conn):
        if exception == 'loop':
            store.event(conn, cid, 'loop_stopped', {})
        if exception == 'stop':
            conn.execute('UPDATE cards SET series_stopped=1 WHERE id=?', (cid,))
        d.finish(conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone(), 'done', 'finished')
        if exception == 'legacy':
            row = conn.execute('SELECT * FROM outbox WHERE card_id=?', (cid,)).fetchone()
            payload = json.loads(row['payload'])
            payload.pop('result')
            conn.execute('UPDATE outbox SET payload=? WHERE card_id=?', (json.dumps(payload), cid))
    assert d.publish() == 1
    row = conn.execute('SELECT * FROM outbox WHERE card_id=?', (cid,)).fetchone()
    assert (d.adapter().inbox.get(row['event_id']) is not None) == (exception != 'legacy')
