import json
import sqlite3

import pytest
from hive import api, store, dispatcher


def create(conn, **kw):
    args = dict(title='Class contract', goal='verify entity classification', acceptance='true', step=1,
                source='msg:123', created_by='secretary')
    args.update(kw)
    return api.create(conn, **args)[0]


def test_explicit_and_fallback(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    owner = create(conn)
    assert api.get(conn, owner)['card_class'] == 'owner_task'
    system = create(conn, source='unknown', card_class='system')
    assert not api.get(conn, system)['owner_visible']
    for kw in (dict(source='unknown'), dict(card_class='bogus'), dict(owner_visible='yes')):
        with pytest.raises(api.CardError):
            create(conn, **kw)
    child = create(conn, source='unknown', followup_of=owner)
    assert api.get(conn, child)['card_class'] == 'owner_task'
    assert api.get(conn, child)['owner_visible'] == 1
    explicit = create(conn, followup_of=owner, card_class='followup', owner_visible=False)
    assert api.get(conn, explicit)['card_class'] == 'followup'
    assert api.listing(conn)[0]['card_class'] == 'followup'
    with pytest.raises(sqlite3.IntegrityError):
        conn.execute('UPDATE cards SET card_class=? WHERE id=?', ('unknown', owner))


def test_series_and_payload(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    sid = create(conn, kind='recurring', every=60, max_runs=2, owner_visible=True)
    d = dispatcher.Dispatcher.__new__(dispatcher.Dispatcher)
    d.conn = conn
    d.cfg = {'card_show_command': 'hive.py show', 'texts': {'result_note': 'result', 'series_news_note': 'news'}}
    assert api.get(conn, sid)['card_class'] == 'series_run'
    payload = d.result_payload(sid, 'done', None)
    assert payload['card_class'] == 'series_run'
    assert payload['owner_visible'] is True
    with pytest.raises(api.CardError):
        create(conn, kind='recurring', every=60, max_runs=2, card_class='owner_task')


def test_old_db_migration_is_idempotent(tmp_path):
    path = tmp_path / 'old.db'
    conn = sqlite3.connect(path)
    conn.executescript(store.SCHEMA)
    for cid, source, creator, kind in [('owner', 'msg:1', 'secretary', 'task'), ('cron', 'odd', 'secretary', 'recurring'), ('unknown', 'odd', 'secretary', 'task')]:
        conn.execute('INSERT INTO cards(id,title,goal,acceptance,step,source,created_by,kind,state,created_at,updated_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)', (cid, cid, 'g', 'true', 1, source, creator, kind, 'done', 1, 1))
    conn.commit()
    conn.close()
    conn = store.connect(path, owner_actor='owner')
    assert api.get(conn, 'owner')['card_class'] == 'owner_task'
    assert conn.execute("SELECT card_class FROM cards WHERE id='cron'").fetchone()[0] == 'series_run'
    assert not conn.execute("SELECT owner_visible FROM cards WHERE id='cron'").fetchone()[0]
    assert json.loads(conn.execute("SELECT data FROM events WHERE card_id='unknown'").fetchone()[0])['ambiguous']
    conn.close()
    conn = store.connect(path)
    assert conn.execute("SELECT COUNT(*) FROM events WHERE kind='class_migrated'").fetchone()[0] == 3
