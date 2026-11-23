"""Per-run tmpfs scratch for heads."""
import os
import time

from hive import runner


def test_tmpdir_created_private_and_env_points_there(tmp_path):
    d = runner.head_tmpdir(42, root=str(tmp_path), min_free_mb=0)
    assert d.parent == tmp_path and d.name.startswith('hive-run-42-')
    assert (d.stat().st_mode & 0o777) == 0o700
    env = runner.with_tmpdir({'HOME': '/h', 'TMPDIR': '/tmp'}, d)
    assert env['HOME'] == '/h'
    assert all(env[k] == str(d) for k in runner.TMP_VARS)


def test_disabled_missing_or_full_root_keeps_default_tmp(tmp_path):
    assert runner.head_tmpdir(1, root='') is None
    assert runner.head_tmpdir(1, root=str(tmp_path / 'absent'), min_free_mb=0) is None
    assert runner.head_tmpdir(1, root=str(tmp_path), min_free_mb=10 ** 12) is None
    env = {'TMPDIR': '/tmp'}
    assert runner.with_tmpdir(env, None) is env


def test_stale_leftovers_swept_fresh_kept(tmp_path):
    old, fresh = tmp_path / 'hive-run-1-x', tmp_path / 'hive-run-2-y'
    old.mkdir(), fresh.mkdir()
    (old / 'f').write_text('x')
    past = time.time() - runner.TMPFS_STALE_S - 60
    os.utime(old, (past, past))
    runner.head_tmpdir(3, root=str(tmp_path), min_free_mb=0)
    assert not old.exists() and fresh.exists()
