"""The board's card query: the same rows, columns and order as the old full scan of cards, read through indexes."""
import itertools
import re
import sqlite3

import pytest
from hive import api, store

NOW = 2_000_000_000.0
DAY = 86400
# The query board() ran before api.BOARD_CARDS: the oracle.
OLD_BOARD_CARDS = """SELECT c.*, p.target_id AS parent, f.target_id AS followup_of
                     FROM cards c LEFT JOIN links p ON p.card_id=c.id AND p.kind='parent'
                     LEFT JOIN links f ON f.card_id=c.id AND f.kind='followup'
                     WHERE c.kind != 'recurring' AND (c.card_class != 'series_run' OR f.target_id IS NOT NULL)
                     ORDER BY c.priority DESC, c.created_at"""
# Every state with the fields it needs for a place on the board (hive-py/AGENTS.md, Board).
STATES = [('created', {}), ('created', {'not_before': NOW + 60}), ('queued', {}), ('queued', {'retry_at': NOW + 60}),
          ('running', {}), ('review', {}), ('check', {}), ('done', {'finished_at': NOW - 60}),
          ('done', {'finished_at': NOW - 3 * DAY}), ('cancelled', {}), ('blocked', {}),
          ('blocked', {'wait_reason': 'stuck'}), ('blocked', {'wait_reason': 'dependency'})]
CLASSES = ('owner_task', 'series_run', 'system', 'followup')


def add_card(conn, cid, created_at, kind='task', card_class='system', state='created', priority=0, **fields):
    cols = dict(id=cid, kind=kind, lane=api.KINDS[kind], title=f'card {cid}', goal='g' * 3000, acceptance='true',
                body='b' * 3000, step=1, source='test', created_by='test', state=state, card_class=card_class,
                priority=priority, created_at=created_at, updated_at=created_at, result='r' * 2000, **fields)
    conn.execute(f"INSERT INTO cards({','.join(cols)}) VALUES({','.join('?' * len(cols))})", tuple(cols.values()))
    return cid


def link(conn, cid, kind, target):
    conn.execute('INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,?,?,?)', (cid, kind, target, NOW))


@pytest.fixture
def mixed(tmp_path):
    """A store with every kind, class and state, series runs with and without a followup link or a parent, recurring
    cards with links, and cards with two followup links."""
    conn = store.connect(tmp_path / 'hive.db')
    at = itertools.count(NOW - 10 * DAY, 7.0)
    rule = add_card(conn, 'rule', next(at), kind='recurring', card_class='series_run', state='blocked',
                    wait_reason='schedule')
    rule2 = add_card(conn, 'rule2', next(at), kind='recurring', card_class='series_run', state='blocked',
                     wait_reason='schedule')
    epic = add_card(conn, 'epic', next(at), kind='epic', card_class='owner_task', state='blocked',
                    wait_reason='children')
    n = 0
    for card_class, (state, fields), priority in itertools.product(CLASSES, STATES, (0, 100)):
        n += 1
        cid = add_card(conn, f'k{n:03d}', next(at), card_class=card_class, state=state, priority=priority, **fields)
        if card_class == 'series_run':
            link(conn, cid, 'series', rule)
        if n % 3 == 0:
            link(conn, cid, 'followup', epic)
        if n % 4 == 0:
            link(conn, cid, 'parent', epic)
        if n % 11 == 0:
            link(conn, cid, 'followup', rule)
        if state == 'blocked' and fields.get('wait_reason') == 'dependency':
            link(conn, cid, 'blocked_by', epic)
        if state in ('running', 'review'):
            conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                            VALUES(?,1,'worker','fake','','',1,'','running',?)""", (cid, NOW - 60))
        if state == 'check' and n % 2:
            conn.execute("INSERT INTO gate_jobs(card_id,version,mode,started_at) VALUES(?,0,'gate',?)", (cid, NOW - 60))
    # Ties in priority and created_at, kept in insertion order.
    tie = next(at)
    for i in range(3):
        add_card(conn, f'tie{i}', tie, card_class='owner_task')
    link(conn, add_card(conn, 'tie-run', tie, card_class='series_run'), 'followup', epic)
    # Recurring cards stay off the board whatever their links.
    link(conn, rule2, 'followup', epic)
    link(conn, rule2, 'parent', epic)
    add_card(conn, 'rule3', next(at), kind='recurring', card_class='system', state='blocked', wait_reason='schedule')
    return conn


def rows(conn, sql):
    return [tuple(r) for r in conn.execute(sql)]


def test_board_cards_are_the_rows_of_the_old_query_in_its_order(mixed):
    old, new = rows(mixed, OLD_BOARD_CARDS), rows(mixed, api.BOARD_CARDS)
    assert new == old
    assert [d[0] for d in mixed.execute(api.BOARD_CARDS).description] == \
           [d[0] for d in mixed.execute(OLD_BOARD_CARDS).description]
    ids = {r[0] for r in new}
    assert {'rule', 'rule2', 'rule3'}.isdisjoint(ids) and 'tie-run' in ids and 'epic' in ids
    runs = {r['id'] for r in mixed.execute("SELECT id FROM cards WHERE card_class='series_run' AND kind='task'")}
    followups = {r['card_id'] for r in mixed.execute("SELECT card_id FROM links WHERE kind='followup'")}
    assert runs & ids == runs & followups and runs - followups
    assert len(new) > len(ids)   # cards with two followup links appear once per link, as before


def test_board_is_the_board_of_the_old_query(mixed, monkeypatch):
    def board():
        return api.board(mixed, NOW, archive_limit=1000, alive=lambda run, cgroup_root: run['id'] % 2 == 1)
    new = board()
    monkeypatch.setattr(api, 'BOARD_CARDS', OLD_BOARD_CARDS)
    assert new == board()
    assert all(new['columns'].values()) and new['archive']


def test_card_class_is_never_null(mixed):
    with pytest.raises(sqlite3.IntegrityError):
        add_card(mixed, 'nullclass', NOW, card_class=None)


def plan(conn, sql):
    return ' | '.join(r[3] for r in conn.execute('EXPLAIN QUERY PLAN ' + sql))


def assert_no_card_scan(conn):
    p = plan(conn, api.BOARD_CARDS)
    # A scan of an index (USING COVERING INDEX) is fine; a scan of the cards table is not.
    assert not re.search(r'SCAN (c|cards)\b(?! USING COVERING INDEX)', p) and 'cards_board' in p, p


def test_board_cards_read_cards_through_indexes(mixed):
    for _ in range(300):
        add_card(mixed, f'run{_}', NOW, card_class='series_run')
    assert_no_card_scan(mixed)
    mixed.execute('ANALYZE')
    assert_no_card_scan(mixed)


def test_a_store_older_than_the_board_indexes_gains_them_on_connect(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    conn.execute('DROP INDEX cards_board')
    conn.execute('DROP INDEX links_followup')
    conn.close()
    conn = store.connect(tmp_path / 'hive.db')
    names = {r[0] for r in conn.execute("SELECT name FROM sqlite_master WHERE type='index'")}
    assert {'cards_board', 'links_followup'} <= names
    assert_no_card_scan(conn)
