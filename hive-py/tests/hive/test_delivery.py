"""Result delivery goes to the owner inbox; the Hive has no direct Telegram sender."""
import json
from pathlib import Path

import pytest
from test_hive import env, hive_cli  # noqa: F401
from hive import api, store


def finished_card(conn, destination='telegram', stdout='', state='done', script=None):
    cid = api.create(conn, title='synthetic', goal='check', acceptance='true', step=1,
                     source='system:test', created_by='secretary', deliver=destination, script=script)[0]
    with store.tx(conn):
        c = conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()
        store.cas_card(conn, c, 'running', round=1)
        conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,report,exit_code)
                        VALUES(?,1,'worker','script','','',1,'','exited',0,?,?)""", (cid, stdout, 0 if state == 'done' else 7))
    return cid


def test_legacy_telegram_result_reaches_inbox_with_full_text(env, tmp_path, monkeypatch):
    _, conn, d = env
    script = tmp_path / 'job.sh'
    script.write_text('exit 0')
    text = 'full collector result ' * 200
    cid = finished_card(conn, stdout=text, script=str(script))
    conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at,report) VALUES(?,1,'reviewer','fake','','',1,'','exited',1,'PASS reviewer')", (cid,))
    # Old pending rows and watch gates may bypass api.create normalization.
    conn.execute("UPDATE cards SET deliver='telegram' WHERE id=?", (cid,))
    conn.commit()
    with store.tx(conn):
        d.finish(conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone(), 'done', 'gate passed')
    assert d.publish() == 1
    assert d.publish() == 0
    event_id = conn.execute('SELECT event_id FROM outbox').fetchone()[0]
    packet = d.inbox().get(event_id)
    assert packet is not None
    assert json.loads(d.inbox().packets[event_id]['payload'])['text'] == text


def test_long_result_text_is_bounded(env):
    _, conn, d = env
    cid = finished_card(conn, stdout='я' * 12000)
    with store.tx(conn):
        d.finish(conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone(), 'done', 'gate passed')
    assert d.publish() == 1
    event_id = conn.execute('SELECT event_id FROM outbox').fetchone()[0]
    payload = json.loads(d.inbox().packets[event_id]['payload'])
    assert payload['text'] == 'я' * 8000
    assert payload['text_truncated'] is True


def test_legacy_api_normalizes_and_local_stays_local(env, monkeypatch):
    _, conn, d = env
    cid = finished_card(conn)
    assert conn.execute('SELECT deliver FROM cards WHERE id=?', (cid,)).fetchone()[0] == 'owner-inbox'
    local = finished_card(conn, destination='local')
    with store.tx(conn):
        d.finish(conn.execute('SELECT * FROM cards WHERE id=?', (local,)).fetchone(), 'done', 'gate passed')
    monkeypatch.setattr(d, 'publish_inbox', lambda *a: pytest.fail('local published'))
    assert d.publish() == 1


def test_cli_flags_and_missing_agent_acceptance(env, tmp_path, monkeypatch):
    cfg, conn, _ = env
    script = tmp_path / 'job.sh'
    script.write_text('exit 0')
    base = ['create', '--title', 'cron', '--goal', 'check', '--step', '1', '--source', 'test', '--created-by', 'secretary']
    assert hive_cli(monkeypatch, cfg, *base, '--cron', '0 8 * * *', '--tz', 'UTC', '--script', str(script),
                    '--script-timeout', '10', '--deliver', 'local') == 0
    card = conn.execute('SELECT * FROM cards').fetchone()
    assert card['kind'] == 'recurring' and card['series_tz'] == 'UTC' and card['script_timeout'] == 10
    with pytest.raises(SystemExit):
        hive_cli(monkeypatch, cfg, *base)
