"""The journal contract of the board for its incremental readers, checked on every store connection of a test.

A reader that follows the board by its id marks (a dashboard importing the board every minute) re-reads only the
cards it saw open and the cards with new events, runs, gates or links. It relies on these rules:

- events are append-only: never updated or deleted;
- cards are never deleted, and gate rows are never updated in place (a retry replaces the row, so it gets a new id);
- every change to a done or cancelled card, every change or removal of a link (journaled on its `card_id`) and every
  change to a run that has ended and been handled writes an event on that card in the same transaction. A new
  card, link, run, gate or event needs no event of its own: its new rowid is past the reader's mark.

`install` adds TEMP triggers to one connection; they change nothing in the board file. `check` pairs the changes
of one transaction with its events and records each breach in `BREACHES`; writes made by test code
(arranging a board) are not the hive's and are skipped.
"""
import contextlib
import sys
from pathlib import Path

BREACHES = []

TRIGGERS = """
CREATE TEMP TABLE IF NOT EXISTS journal_watch(seq INTEGER PRIMARY KEY, what TEXT NOT NULL, card_id TEXT, by_test INTEGER);
CREATE TEMP TRIGGER IF NOT EXISTS jw_event AFTER INSERT ON main.events
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('event', NEW.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_event_update AFTER UPDATE ON main.events
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('event updated', OLD.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_event_delete AFTER DELETE ON main.events
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('event deleted', OLD.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_card_delete AFTER DELETE ON main.cards
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('card deleted', OLD.id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_final_card AFTER UPDATE ON main.cards WHEN OLD.state IN ('done', 'cancelled')
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('final card changed', OLD.id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_link_update AFTER UPDATE ON main.links
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('link changed', OLD.card_id, jw_by_test());
        INSERT INTO journal_watch(what, card_id, by_test) VALUES('link changed', NEW.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_link_delete AFTER DELETE ON main.links
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('link removed', OLD.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_gate_update AFTER UPDATE ON main.gates
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('gate updated', OLD.card_id, jw_by_test()); END;
CREATE TEMP TRIGGER IF NOT EXISTS jw_handled_run AFTER UPDATE ON main.runs
  WHEN OLD.handled = 1 AND OLD.status NOT IN ('starting', 'running')
  BEGIN INSERT INTO journal_watch(what, card_id, by_test) VALUES('handled run changed', OLD.card_id, jw_by_test()); END;
"""
NEVER = {'event updated', 'event deleted', 'card deleted', 'gate updated'}


TESTS = Path(__file__).resolve().parent


def _by_test():
    """1 when the statement firing the trigger was executed by test code (a test arranging its board), else 0."""
    frame = sys._getframe(1)
    return int(frame is not None and Path(frame.f_code.co_filename).resolve().parent == TESTS)


def install(conn):
    conn.create_function('jw_by_test', 0, _by_test)
    conn.executescript(TRIGGERS)
    return conn


def check(conn, where):
    """Pair the changes since the last check with the events written alongside them; record every breach."""
    try:
        rows = conn.execute('SELECT what, card_id FROM temp.journal_watch WHERE NOT by_test ORDER BY seq').fetchall()
        conn.execute('DELETE FROM temp.journal_watch')
    except Exception:  # a closed connection or one without the triggers has nothing left to check
        return
    journaled = {card for what, card in rows if what == 'event'}
    for what, card in rows:
        if what in NEVER or (what != 'event' and card not in journaled):
            BREACHES.append(f'{what}: {card} ({where})')


def checked_tx(tx):
    """store.tx that checks the autocommit writes before it and its own writes before COMMIT."""
    @contextlib.contextmanager
    def wrapped(conn, *args, **kwargs):
        check(conn, 'outside a transaction')
        with tx(conn, *args, **kwargs) as c:
            yield c
            check(conn, 'transaction')
    return wrapped
