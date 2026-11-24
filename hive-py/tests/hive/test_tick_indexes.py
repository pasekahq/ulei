"""The dispatcher tick reads hive.db through indexes, not full table scans."""
import re

from hive import store


def plan(conn, sql, args=()):
    return ' | '.join(r[3] for r in conn.execute('EXPLAIN QUERY PLAN ' + sql, args))


def test_hot_tick_queries_use_indexes(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    names = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='index'")}
    assert {'cards_by_state', 'cards_recheck_due', 'cards_series_member', 'runs_by_card',
            'runs_by_status', 'runs_unhandled', 'outbox_unpublished'} <= names
    hot = {
        "SELECT * FROM runs WHERE handled=0 AND status IN ('exited','timeout') ORDER BY id": 'runs_unhandled',
        "SELECT 1 FROM runs WHERE card_id='x' LIMIT 1": 'runs_by_card',
        "SELECT * FROM outbox WHERE published_at IS NULL ORDER BY created_at": 'outbox_unpublished',
        "SELECT * FROM cards WHERE state='created'": 'cards_by_state',
        "SELECT * FROM cards WHERE state='done' AND recheck_due IS NOT NULL AND recheck_due<=1": 'cards_recheck_due',
        """SELECT count(*) FROM runs r JOIN cards c ON c.id=r.card_id
           WHERE r.status IN ('starting','running') AND r.vendor IS NOT c.executor""": 'runs_by_status',
    }
    for sql, index in hot.items():
        p = plan(conn, sql)
        # A partial index may be walked ("SCAN outbox USING INDEX ..."); a bare table scan may not.
        assert index in p and not re.search(r'SCAN \w+( AS \w+)?( \||$)', p), (sql, p)
    p = plan(conn, """SELECT id, card_id FROM events WHERE card_id=(SELECT card_id FROM runs WHERE id=1)
                      AND kind='claimed' AND json_extract(data, '$.run')=1""")
    assert 'events_card' in p
    p = plan(conn, """SELECT c.id FROM links l JOIN cards c INDEXED BY cards_series_member ON c.id=l.card_id
                      WHERE l.target_id='s' AND l.kind='series' ORDER BY c.series_period""")
    assert 'COVERING INDEX cards_series_member' in p


def test_instance_states_match_instances(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    now = 1.0
    common = dict(title='t', goal='g' * 5000, acceptance='true', step=1, source='test', created_by='test',
                  created_at=now, updated_at=now)
    def card(cid, state, period=None, wait=None, result=None):
        cols = dict(common, id=cid, state=state, series_period=period, wait_reason=wait, result=result)
        conn.execute(f"INSERT INTO cards({','.join(cols)}) VALUES({','.join('?' * len(cols))})", tuple(cols.values()))
    card('s', 'blocked', wait='schedule')
    for i, (state, wait) in enumerate([('done', None), ('blocked', None), ('blocked', 'dependency'), ('queued', None)]):
        card(f'i{i}', state, period=10 - i, wait=wait, result='r' * 100)
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series','s',?)", (f'i{i}', now))
    full = store.instances(conn, 's')
    light = store.instance_states(conn, 's')
    assert [i['id'] for i in full] == ['i3', 'i2', 'i1', 'i0']
    assert light == [{k: i[k] for k in ('id', 'period', 'state', 'wait_reason')} for i in full]
    assert [store.is_final(i) for i in light] == [store.is_final(i) for i in full]
