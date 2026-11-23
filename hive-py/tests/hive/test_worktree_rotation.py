"""Worktree rotation: only clean, pushed checkouts of old final cards go away."""
from datetime import datetime
import importlib.util
import json
from pathlib import Path
import sqlite3
import subprocess
from zoneinfo import ZoneInfo

import pytest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('rotation', ROOT / 'scripts' / 'hive_worktree_rotation.py')
rotation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rotation)

TZ = 'UTC'
NOON = datetime(2026, 11, 16, 12, 0, tzinfo=ZoneInfo(TZ)).timestamp()
OLD = NOON - 7 * 3600


def run(*args, cwd=None):
    subprocess.run(['git', *args], cwd=cwd, check=True, capture_output=True,
                   env={'GIT_AUTHOR_NAME': 't', 'GIT_AUTHOR_EMAIL': 't@t', 'GIT_COMMITTER_NAME': 't',
                        'GIT_COMMITTER_EMAIL': 't@t', 'HOME': str(cwd or '/tmp'), 'PATH': '/usr/bin:/bin'})


@pytest.fixture
def world(tmp_path):
    remote = tmp_path / 'remote.git'
    run('init', '-q', '--bare', '-b', 'main', str(remote))
    seed = tmp_path / 'seed'
    run('clone', '-q', str(remote), str(seed))
    (seed / 'a').write_text('a')
    run('add', 'a', cwd=seed)
    run('commit', '-qm', 'a', cwd=seed)
    run('push', '-q', 'origin', 'HEAD:main', cwd=seed)
    cards = tmp_path / 'cards'
    cards.mkdir()
    db = tmp_path / 'hive.db'
    conn = sqlite3.connect(db)
    conn.execute('CREATE TABLE cards(id TEXT PRIMARY KEY, state TEXT, wait_reason TEXT, '
                 'finished_at REAL, updated_at REAL)')
    conn.commit()

    def card(cid, state='done', finished=OLD, wait=None):
        conn.execute('INSERT INTO cards VALUES(?,?,?,?,?)', (cid, state, wait, finished, finished))
        conn.commit()
        (cards / cid).mkdir()
        return cards / cid

    def clone(cid, name='repo', **kw):
        path = card(cid, **kw) / name
        run('clone', '-q', str(remote), str(path))
        return path

    def worktree(cid, branch, **kw):
        path = card(cid, **kw) / 'repo'
        run('worktree', 'add', '-q', '-b', branch, str(path), 'origin/main', cwd=seed)
        return path

    return {'tmp': tmp_path, 'db': db, 'cards': cards, 'seed': seed, 'card': card, 'clone': clone,
            'worktree': worktree, 'log': tmp_path / 'rot.jsonl'}


def rotate(w, apply=True, now=NOON):
    return rotation.rotate(w['db'], w['cards'], w['log'], TZ, apply=apply, now=now)


def test_removes_clean_pushed_checkouts_and_keeps_the_rest(world):
    w = world
    pushed_wt = w['worktree']('h_wt', 'b1')
    dirty_wt = w['worktree']('h_dirty', 'b2')
    (dirty_wt / 'a').write_text('changed')
    unpushed_wt = w['worktree']('h_unpushed', 'b3')
    (unpushed_wt / 'b').write_text('b')
    run('add', 'b', cwd=unpushed_wt)
    run('commit', '-qm', 'b', cwd=unpushed_wt)
    untracked = w['clone']('h_untracked')
    (untracked / 'new').write_text('x')
    clean = w['clone']('h_clone')
    young = w['clone']('h_young', finished=NOON - 3600)
    running = w['clone']('h_running', state='running')
    waiting = w['clone']('h_waiting', state='blocked', wait=json.dumps('dependency'))
    final_blocked = w['clone']('h_failed', state='blocked')

    res = rotate(w)

    gone = {Path(i['path']) for i in res['removed']}
    assert gone == {pushed_wt, clean, final_blocked}
    for p in gone:
        assert not p.exists()
    for p in (dirty_wt, unpushed_wt, untracked, young, running, waiting):
        assert p.exists()
    listed = subprocess.run(['git', 'worktree', 'list'], cwd=w['seed'], capture_output=True, text=True).stdout
    assert str(pushed_wt) not in listed and str(dirty_wt) in listed
    reasons = {Path(i['path']).parent.name: i['reason'] for i in res['skipped']}
    assert 'dirty' in reasons['h_dirty'] and 'not on any remote' in reasons['h_unpushed']
    assert 'dirty' in reasons['h_untracked']
    log = [json.loads(l) for l in w['log'].read_text().splitlines()]
    assert {l['action'] for l in log} == {'removed', 'skipped'} and len(log) == 6


def test_clone_with_unpushed_branch_stash_or_lent_objects_is_kept(world):
    w = world
    branchy = w['clone']('h_branch')
    run('checkout', '-qb', 'side', cwd=branchy)
    (branchy / 'c').write_text('c')
    run('add', 'c', cwd=branchy)
    run('commit', '-qm', 'c', cwd=branchy)
    run('checkout', '-q', 'main', cwd=branchy)
    stashed = w['clone']('h_stash')
    (stashed / 'a').write_text('stash me')
    run('stash', '-q', cwd=stashed)
    lender = w['clone']('h_lender')
    borrower = w['card']('h_borrower', state='running') / 'repo'
    run('clone', '-q', '--reference', str(lender), str(w['tmp'] / 'remote.git'), str(borrower))

    res = rotate(w)

    assert not res['removed']
    reasons = {Path(i['path']).parent.name: i['reason'] for i in res['skipped']}
    assert 'local branch' in reasons['h_branch']
    assert 'stash' in reasons['h_stash']
    assert 'alternates' in reasons['h_lender']
    assert lender.exists() and borrower.exists()


def test_clone_hosting_a_worktree_goes_only_after_the_worktree(world):
    w = world
    host = w['clone']('h_host')
    guest = w['card']('h_guest') / 'repo'
    run('worktree', 'add', '-q', '-b', 'g', str(guest), 'origin/main', cwd=host)
    keep = w['card']('h_keep') / 'repo'
    host2 = w['clone']('h_host2')
    run('worktree', 'add', '-q', '-b', 'k', str(keep), 'origin/main', cwd=host2)
    (keep / 'a').write_text('dirty')

    res = rotate(w)

    assert {Path(i['path']) for i in res['removed']} == {guest, host}
    assert host2.exists() and keep.exists()
    assert 'other worktree' in next(i['reason'] for i in res['skipped'] if Path(i['path']) == host2)


def test_dry_run_and_backup_window_touch_nothing(world):
    w = world
    clean = w['clone']('h_clone')
    res = rotate(w, apply=False)
    assert [i['action'] for i in res['removed']] == ['would-remove'] and clean.exists()
    night = datetime(2026, 11, 17, 3, 30, tzinfo=ZoneInfo(TZ)).timestamp()
    res = rotate(w, now=night)
    assert res['skipped_window'] and clean.exists()
    assert rotation.in_window(datetime(2026, 11, 17, 4, 59, tzinfo=ZoneInfo(TZ)).timestamp(), TZ)
    assert not rotation.in_window(datetime(2026, 11, 17, 5, 0, tzinfo=ZoneInfo(TZ)).timestamp(), TZ)
