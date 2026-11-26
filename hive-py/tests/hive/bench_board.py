"""Bench of the board's card query, not collected by pytest: bytes read (rchar of /proc/self/io), read calls and
time of the old full-scan query against api.BOARD_CARDS on a synthetic store.

    python hive-py/tests/hive/bench_board.py [--cards 20000] [--series 0.95] [--body 3000] [--dir DIR]

Each query runs on a fresh connection, so SQLite's own page cache starts empty; the OS page cache stays warm, which
rchar does not see. The card_class column comes after the long text columns, as on a store migrated to card_class.
"""
import argparse
import random
import sqlite3
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from hive import api, store  # noqa: E402

OLD_BOARD_CARDS = """SELECT c.*, p.target_id AS parent, f.target_id AS followup_of
                     FROM cards c LEFT JOIN links p ON p.card_id=c.id AND p.kind='parent'
                     LEFT JOIN links f ON f.card_id=c.id AND f.kind='followup'
                     WHERE c.kind != 'recurring' AND (c.card_class != 'series_run' OR f.target_id IS NOT NULL)
                     ORDER BY c.priority DESC, c.created_at"""


def io():
    fields = dict(line.split(': ') for line in Path('/proc/self/io').read_text().splitlines())
    return int(fields['rchar']), int(fields['syscr'])


def build(path, cards, series, body, seed=1):
    rnd = random.Random(seed)
    conn = store.connect(path)
    now = 1_700_000_000.0
    text = 'x' * body
    rules = [f'rule{i}' for i in range(20)]
    with store.tx(conn):
        for rule in rules:
            conn.execute("""INSERT INTO cards(id,kind,lane,title,goal,acceptance,body,step,source,created_by,state,
                            wait_reason,card_class,created_at,updated_at) VALUES(?,'recurring','recurring','r','g','true',
                            '',1,'test','test','blocked','schedule','series_run',?,?)""", (rule, now, now))
        for i in range(cards):
            run = rnd.random() < series
            state = 'done' if run or rnd.random() < 0.9 else rnd.choice(('created', 'queued', 'blocked'))
            cid = f'c{i:06d}'
            conn.execute("""INSERT INTO cards(id,kind,lane,title,goal,acceptance,body,step,source,created_by,state,
                            result,feedback,card_class,priority,created_at,updated_at,finished_at)
                            VALUES(?,'task','task','t',?,'true',?,1,'test','test',?,?,?,?,0,?,?,?)""",
                         (cid, text[:body // 3], text, state, text[:body // 3], text[:body // 6],
                          'series_run' if run else 'system', now + i, now + i, now + i if state == 'done' else None))
            if run:
                conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)",
                             (cid, rnd.choice(rules), now))
            elif i and rnd.random() < 0.05:
                conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'followup',?,?)",
                             (cid, f'c{rnd.randrange(i):06d}', now))
    conn.close()


def measure(path, sql):
    conn = sqlite3.connect(path)
    r0, c0 = io()
    t0 = time.perf_counter()
    rows = conn.execute(sql).fetchall()
    dt = time.perf_counter() - t0
    r1, c1 = io()
    plan = ' | '.join(r[3] for r in conn.execute('EXPLAIN QUERY PLAN ' + sql))
    conn.close()
    return rows, r1 - r0, c1 - c0, dt, plan


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--cards', type=int, default=20000)
    ap.add_argument('--series', type=float, default=0.95)
    ap.add_argument('--body', type=int, default=3000)
    ap.add_argument('--dir', default=None)
    a = ap.parse_args()
    with tempfile.TemporaryDirectory(dir=a.dir) as tmp:
        path = Path(tmp) / 'hive.db'
        build(path, a.cards, a.series, a.body)
        print(f'store: {path.stat().st_size / 1e6:.1f} MB, {a.cards} cards, {a.series:.0%} series_run, '
              f'body {a.body} B')
        old = measure(path, OLD_BOARD_CARDS)
        new = measure(path, api.BOARD_CARDS)
        for name, (rows, rchar, syscr, dt, plan) in (('old', old), ('new', new)):
            print(f'{name}: {len(rows)} rows, rchar {rchar / 1e6:.2f} MB, {syscr} read calls, {dt * 1000:.0f} ms')
            print(f'  plan: {plan}')
        assert sorted(map(tuple, old[0])) == sorted(map(tuple, new[0])), 'old and new rows differ'


if __name__ == '__main__':
    main()
