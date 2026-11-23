#!/usr/bin/env python3
"""Rotate git checkouts left behind by finished Hive cards.

A card workspace is CARDS/<card_id>/. Heads often leave a git checkout one
level below it (repo/, source/, worktree/ ...): either a linked worktree of a
repository elsewhere or a full clone. Each costs ~200 MB and is useless once
the card is final and its commits are on a remote.

Rules (anything else is skipped and logged, never forced):
  * the card is final in hive.db (done, cancelled, or blocked with no
    wait_reason) and finished more than --min-age-hours ago;
  * the checkout has no uncommitted or untracked changes;
  * every local commit (HEAD; for clones every branch too) is reachable from a
    remote-tracking ref, and a clone has no stash;
  * a clone that still hosts other worktrees is kept;
  * linked worktrees go through ``git worktree remove`` (no --force), clones
    are deleted only after the same checks;
  * nothing runs inside the nightly backup window (03:00-05:00 local).

Output: one JSON line per checkout to --log, a summary JSON on stdout. --db, --cards, --log and --tz default to
the config keys db, workspaces, rotation_log and owner_tz of --config or HIVE_CONFIG.
"""
import argparse
from datetime import datetime
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import time
from zoneinfo import ZoneInfo

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from hive import store  # noqa: E402

WINDOW = (3, 5)  # [start, end) hours, local time: nightly backup
GIT_TIMEOUT = 60


class Skip(Exception):
    pass


def in_window(now, tz, window=WINDOW):
    hour = datetime.fromtimestamp(now, ZoneInfo(tz)).hour
    return window[0] <= hour < window[1]


def final_cards(db, cutoff):
    """{card_id: finished_at} for final cards finished before cutoff (read-only)."""
    conn = sqlite3.connect(Path(db).resolve().as_uri() + '?mode=ro', uri=True, timeout=30)
    try:
        rows = conn.execute(
            "SELECT id, COALESCE(finished_at, updated_at) FROM cards "
            "WHERE (state IN ('done','cancelled') OR (state='blocked' AND wait_reason IS NULL)) "
            "AND COALESCE(finished_at, updated_at) < ?", (cutoff,)).fetchall()
    finally:
        conn.close()
    return dict(rows)


def git(path, *args, check=True):
    p = subprocess.run(['git', '-C', str(path), *args], capture_output=True, text=True,
                       timeout=GIT_TIMEOUT, env={**os.environ, 'GIT_OPTIONAL_LOCKS': '0',
                                                 'GIT_TERMINAL_PROMPT': '0'})
    if check and p.returncode:
        raise Skip(f"git {' '.join(args)}: {(p.stderr or p.stdout).strip()[:200]}")
    return p.stdout


def checkouts(cards_dir, ids):
    """(card_id, path) for every git checkout directly under a final card's workspace."""
    for cid in sorted(ids):
        try:
            entries = list(os.scandir(Path(cards_dir) / cid))
        except (FileNotFoundError, NotADirectoryError):
            continue
        for e in sorted(entries, key=lambda e: e.name):
            if e.is_dir(follow_symlinks=False) and os.path.lexists(os.path.join(e.path, '.git')):
                yield cid, Path(e.path)


def borrowed_objects(cards_dir):
    """Object dirs other clones under cards_dir borrow via objects/info/alternates.

    Clones made with --reference/--shared read objects from another clone; deleting
    the lender silently corrupts the borrower, so lenders are never removed.
    """
    lent = set()
    for card in os.scandir(cards_dir):
        try:
            subs = list(os.scandir(card.path)) if card.is_dir(follow_symlinks=False) else []
        except OSError:
            continue
        for sub in subs:
            objects = Path(sub.path) / '.git' / 'objects'
            try:
                lines = (objects / 'info' / 'alternates').read_text().splitlines()
            except OSError:
                continue
            for line in lines:
                line = line.strip()
                if line and not line.startswith('#'):
                    lent.add(os.path.realpath(objects / line))
    return lent


def inspect(path, lent=frozenset()):
    """Return ('worktree'|'clone', common_dir) or raise Skip with the reason."""
    if git(path, 'rev-parse', '--show-toplevel').strip() != str(path.resolve()):
        raise Skip('not the top of a git checkout')
    dirty = git(path, 'status', '--porcelain', '--untracked-files=normal')
    if dirty.strip():
        raise Skip(f'dirty tree: {len(dirty.splitlines())} changed/untracked paths')
    if not git(path, 'remote').split():
        raise Skip('no remote')
    common = Path(git(path, 'rev-parse', '--path-format=absolute', '--git-common-dir').strip())
    linked = (path / '.git').is_file()
    if git(path, 'rev-parse', '--verify', '-q', 'HEAD', check=False).strip():
        if git(path, 'rev-list', '-n1', 'HEAD', '--not', '--remotes').strip():
            raise Skip('HEAD has commits not on any remote')
    if linked:
        return 'worktree', common
    if os.path.realpath(common / 'objects') in lent:
        raise Skip('other clones borrow its objects (alternates)')
    if git(path, 'rev-list', '-n1', '--branches', '--not', '--remotes').strip():
        raise Skip('local branch has commits not on any remote')
    if git(path, 'stash', 'list').strip():
        raise Skip('clone has a stash')
    others = live_worktrees(path)
    if others:
        raise Skip(f'clone hosts {len(others)} other worktree(s): {others[0]}')
    return 'clone', common


def live_worktrees(path):
    """Other worktrees of this repository whose directories still exist."""
    blocks = git(path, 'worktree', 'list', '--porcelain').strip().split('\n\n')
    found = []
    for block in blocks:
        lines = block.splitlines()
        if not lines or not lines[0].startswith('worktree '):
            continue
        wt = Path(lines[0][9:])
        if wt == path.resolve() or any(l.startswith('prunable') for l in lines) or not wt.exists():
            continue
        found.append(str(wt))
    return found


def remove(path, kind, common):
    if kind == 'worktree':
        main = common.parent if common.name == '.git' else common
        git(main, 'worktree', 'remove', str(path))
    else:
        shutil.rmtree(path)


def rotate(db, cards_dir, log, tz, min_age_hours=6.0, apply=False, now=None):
    now = time.time() if now is None else now
    if in_window(now, tz):
        return {'at': now, 'skipped_window': True, 'removed': [], 'skipped': []}
    ids = final_cards(db, now - min_age_hours * 3600)
    removed, skipped = [], []
    lent = borrowed_objects(cards_dir)
    Path(log).parent.mkdir(parents=True, exist_ok=True)
    with open(log, 'a', encoding='utf-8') as out:
        # Linked worktrees first, so a clone hosting them can go in the same run.
        found = sorted(checkouts(cards_dir, ids), key=lambda cp: not (cp[1] / '.git').is_file())
        for cid, path in found:
            item = {'at': round(now), 'card': cid, 'path': str(path)}
            try:
                kind, common = inspect(path, lent)
                item.update(kind=kind, common=str(common))
                if apply:
                    remove(path, kind, common)
                item['action'] = 'removed' if apply else 'would-remove'
                removed.append(item)
            except (Skip, OSError, subprocess.TimeoutExpired) as exc:
                item.update(action='skipped', reason=str(exc) or type(exc).__name__)
                skipped.append(item)
            out.write(json.dumps(item, ensure_ascii=False) + '\n')
    return {'at': now, 'apply': apply, 'final_cards': len(ids), 'removed': removed, 'skipped': skipped}


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split('\n')[0])
    ap.add_argument('--config')
    ap.add_argument('--db')
    ap.add_argument('--cards')
    ap.add_argument('--log')
    ap.add_argument('--min-age-hours', type=float, default=6.0)
    ap.add_argument('--tz')
    ap.add_argument('--apply', action='store_true', help='remove; default is a dry run')
    a = ap.parse_args(argv)
    cfg = store.load_config(a.config)
    res = rotate(a.db or cfg['db'], a.cards or cfg['workspaces'], a.log or cfg['rotation_log'], a.tz or cfg['owner_tz'],
                 a.min_age_hours, a.apply)
    print(json.dumps({'apply': res.get('apply'), 'skipped_window': res.get('skipped_window', False),
                      'final_cards': res.get('final_cards', 0), 'removed': len(res['removed']),
                      'skipped': len(res['skipped']),
                      'skip_reasons': [f"{s['card']}/{Path(s['path']).name}: {s['reason']}"
                                       for s in res['skipped']]}, ensure_ascii=False))
    return 0


if __name__ == '__main__':
    sys.exit(main())
