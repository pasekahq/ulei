"""hive.db write lock: short indexed transactions, busy waits, retries.

A head runner (IOSchedulingClass=idle) that acknowledges a live steer inside BEGIN IMMEDIATE with an
unindexed scan of the whole events journal holds the write lock for minutes under disk contention, and
every dispatcher tick and CLI write fails with 'database is locked'."""
import multiprocessing
import re
import sqlite3
import threading
import time

import pytest

from hive import live, store


def plan(conn, sql, args=()):
    return ' | '.join(r[3] for r in conn.execute('EXPLAIN QUERY PLAN ' + sql, args))


def bare_scan(p, table):
    return re.search(rf'SCAN {table}( AS \w+)?( \||$)', p)


def test_connections_are_wal_with_long_busy_timeout(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    assert conn.execute('PRAGMA journal_mode').fetchone()[0] == 'wal'
    assert conn.execute('PRAGMA busy_timeout').fetchone()[0] >= 30000


def test_live_acknowledge_runs_indexed_under_the_write_lock(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    p = plan(conn, live.ACTIVE_RUN_SQL, (1,))
    assert not bare_scan(p, 'events') and not bare_scan(p, 'e'), p
    assert 'events_card' in p or 'events_steer_stop' in p, p


def test_steer_stop_lookups_use_an_index(tmp_path):
    conn = store.connect(tmp_path / 'hive.db')
    p = plan(conn, """SELECT e.card_id, e.at, r.id AS run FROM events e JOIN runs r ON r.id=json_extract(e.data, '$.run')
                      WHERE e.kind='steer_stop' AND r.status IN ('starting','running')""")
    assert 'events_steer_stop' in p and not bare_scan(p, 'e'), p


def _hold(db, seconds, ready):
    c = sqlite3.connect(db, isolation_level=None)
    c.execute('BEGIN IMMEDIATE')
    c.execute("INSERT INTO events(card_id,at,kind,actor) VALUES('holder',0,'hold','test')")
    ready.set()
    time.sleep(seconds)
    c.execute('COMMIT')


def _write(db, n, out):
    try:
        conn = store.connect(db)
        for i in range(n):
            with store.tx(conn):
                store.event(conn, 'w', 'write', {'i': i}, actor='test')
        out.put('ok')
    except Exception as e:  # pragma: no cover - reported to the parent
        out.put(repr(e))


def test_concurrent_writers_wait_for_a_held_lock_instead_of_failing(tmp_path):
    db = str(tmp_path / 'hive.db')
    store.connect(db).close()
    ctx = multiprocessing.get_context('spawn')
    ready, out = ctx.Event(), ctx.Queue()
    holder = ctx.Process(target=_hold, args=(db, 2, ready))
    holder.start()
    assert ready.wait(10)
    writers = [ctx.Process(target=_write, args=(db, 20, out)) for _ in range(4)]
    for w in writers:
        w.start()
    results = [out.get(timeout=60) for _ in writers]
    for p in [holder, *writers]:
        p.join(30)
    assert results == ['ok'] * 4
    conn = store.connect(db)
    assert conn.execute("SELECT count(*) FROM events WHERE kind='write'").fetchone()[0] == 80


def test_begin_retries_after_a_busy_timeout(tmp_path, capsys):
    db = str(tmp_path / 'hive.db')
    conn = store.connect(db)
    conn.execute('PRAGMA busy_timeout=300')
    other = sqlite3.connect(db, isolation_level=None, check_same_thread=False)
    other.execute('BEGIN IMMEDIATE')
    with pytest.raises(sqlite3.OperationalError, match='locked'):
        with store.tx(conn, retries=0):
            pass
    threading.Timer(0.5, lambda: other.execute('COMMIT')).start()
    with store.tx(conn, retries=5):
        store.event(conn, 'c', 'after_retry', actor='test')
    assert 'retrying the write' in capsys.readouterr().err
    assert conn.execute("SELECT count(*) FROM events WHERE kind='after_retry'").fetchone()[0] == 1


def test_slow_transactions_are_reported(tmp_path, capsys, monkeypatch):
    conn = store.connect(tmp_path / 'hive.db')
    monkeypatch.setattr(store, 'SLOW_TX_S', 0.0)
    with store.tx(conn):
        store.event(conn, 'c', 'slow', actor='test')
    assert 'slow write transaction held hive.db' in capsys.readouterr().err


@pytest.mark.skipif(store._ioprio() is None, reason='ioprio syscalls unavailable')
def test_idle_io_thread_is_lifted_only_while_it_holds_the_lock(tmp_path):
    store.connect(tmp_path / 'hive.db').close()
    conn = sqlite3.connect(tmp_path / 'hive.db', isolation_level=None, check_same_thread=False)
    idle = store._IOPRIO_CLASS_IDLE << store._IOPRIO_CLASS_SHIFT
    seen = {}

    def worker():  # a fresh thread, so the test process keeps its own class
        assert store._ioprio(idle)
        with store.tx(conn):
            seen['inside'] = store._ioprio() >> store._IOPRIO_CLASS_SHIFT
        seen['after'] = store._ioprio()
    t = threading.Thread(target=worker)
    t.start()
    t.join(10)
    assert seen == {'inside': store._IOPRIO_CLASS_BE, 'after': idle}


def test_cli_connections_retry_begin_and_the_daemon_does_not(tmp_path):
    """The daemon retries on its next tick; the CLI rides out a stuck writer."""
    assert store.connect(tmp_path / 'hive.db').begin_retries == 0
    assert store.connect(tmp_path / 'hive.db', begin_retries=2).begin_retries == 2
