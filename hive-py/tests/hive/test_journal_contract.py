"""The board's journal contract for incremental readers (journal_watch.py) catches a silent change by hive code."""
import pytest

import journal_watch
from hive import store


def hive_code(source):
    """Statements compiled as if they were hive code: the contract skips writes made by tests."""
    scope = {}
    exec(compile(source, store.__file__, 'exec'), scope)
    return scope


@pytest.fixture
def board(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    now = 1.0
    common = dict(title='t', goal='g', acceptance='true', step=1, source='test', created_by='test', created_at=now, updated_at=now)
    for cid, state in (('done1', 'done'), ('open1', 'running'), ('epic', 'blocked')):
        cols = dict(common, id=cid, state=state)
        conn.execute(f"INSERT INTO cards({','.join(cols)}) VALUES({','.join('?' * len(cols))})", tuple(cols.values()))
    conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES('open1','parent','epic',1)")
    conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,handled,started_at) "
                 "VALUES('done1',1,'worker','v','m','e',1,'p','exited',1,1)")
    conn.execute("INSERT INTO events(card_id,at,kind) VALUES('done1',1,'created')")
    yield conn
    conn.close()


def breaches():
    found = list(journal_watch.BREACHES)
    journal_watch.BREACHES.clear()
    return found


def test_journaled_changes_keep_the_contract(board):
    card = board.execute("SELECT * FROM cards WHERE id='done1'").fetchone()
    with store.tx(board):
        assert store.cas_card(board, card, 'queued')
        store.event(board, 'done1', 'reopened')
    with store.tx(board):
        store.cas_card(board, board.execute("SELECT * FROM cards WHERE id='open1'").fetchone(), 'check')  # open: no event needed
    assert breaches() == []


def test_silent_change_to_a_final_card_is_a_breach(board):
    with store.tx(board):
        assert store.cas_card(board, board.execute("SELECT * FROM cards WHERE id='done1'").fetchone(), 'queued')
    assert breaches() == ['final card changed: done1 (transaction)']


@pytest.mark.parametrize('statement, breach', [
    ("DELETE FROM links WHERE card_id='open1'", 'link removed: open1'),
    ("UPDATE links SET optional=1 WHERE card_id='open1'", 'link changed: open1'),
    ("UPDATE runs SET error='late' WHERE card_id='done1'", 'handled run changed: done1'),
    ("UPDATE events SET data='x' WHERE card_id='done1'", 'event updated: done1'),
    ("DELETE FROM events WHERE card_id='done1'", 'event deleted: done1'),
])
def test_unjournaled_link_run_or_event_change_is_a_breach(board, statement, breach):
    write = hive_code(f'def write(conn):\n    conn.execute("{statement}")\n')['write']
    with store.tx(board):
        write(board)
    assert breach + ' (transaction)' in breaches()


def test_writes_by_test_code_are_not_the_hives(board):
    with store.tx(board):
        board.execute("UPDATE cards SET result='arranged' WHERE id='done1'")
    assert breaches() == []
