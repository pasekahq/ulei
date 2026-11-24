"""Board projection: columns, badges, the done window, the archive and the cards kept off the board."""
import json
import os

import pytest
from test_hive import env, mk, mk_epic, mk_series  # noqa: F401
from test_liveness import UNIT, manager
from hive import api

NOW = 2_000_000_000.0
DAY = 86400
LIST_KEYS = {'id', 'kind', 'card_class', 'owner_visible', 'state', 'wait_reason', 'step', 'review', 'round', 'title',
             'created_by', 'source', 'assignee', 'executor', 'on_unavailable', 'model', 'effort', 'panel', 'created_at',
             'updated_at', 'parent', 'series', 'series_period', 'admission_class'}


def probe(*live):
    """A fake process probe: the runs with ids in `live` have a live process."""
    return lambda run, cgroup_root: run['id'] in live


def places(conn, cid, now=NOW, alive=probe()):
    """Every place of the card at `now` as (column or 'archive', entry)."""
    b = api.board(conn, now, archive_limit=1000, alive=alive)
    found = [(name, e) for name, entries in b['columns'].items() for e in entries if e['id'] == cid]
    return found + [('archive', e) for e in b['archive'] if e['id'] == cid]


def set_state(conn, cid, state, **fields):
    sets = ', '.join(f'{k}=?' for k in ('state', *fields))
    conn.execute(f'UPDATE cards SET {sets} WHERE id=?', (state, *fields.values(), cid))


def only_instance(conn, rule):
    [instance] = [i['id'] for i in api.get(conn, rule)['instances']]
    return instance


def add_run(conn, cid, role='worker', status='running', pid=None, unit=None):
    return conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,pid,unit,started_at)
                           VALUES(?,1,?,'fake','','',1,'',?,?,?,?)""", (cid, role, status, pid, unit, NOW - 60)).lastrowid


def place(conn, cid, alive=probe()):
    """(column, badge, cause, until) of a card placed in a column."""
    [(column, entry)] = places(conn, cid, alive=alive)
    return column, entry.get('badge'), entry.get('cause'), entry.get('until')


IN_PROGRESS = ('in_progress', None, None, None)
SLOT = ('waiting', 'slot', 'queue', None)


@pytest.mark.parametrize('status', ['starting', 'running'])
def test_running_worker_with_a_live_process_is_in_progress(env, status):
    _, conn, _ = env
    cid = mk(conn, 'working')
    set_state(conn, cid, 'running')
    rid = add_run(conn, cid, status=status)
    assert place(conn, cid, probe(rid)) == IN_PROGRESS


def test_running_card_whose_process_is_gone_before_the_reaper_waits_for_a_slot(env):
    _, conn, _ = env
    cid = mk(conn, 'crashed head')
    set_state(conn, cid, 'running')
    add_run(conn, cid)
    assert place(conn, cid) == SLOT


def test_panel_retry_in_backoff_waits_for_its_retry_time(env):
    _, conn, _ = env
    cid = mk(conn, 'panel member failed')
    set_state(conn, cid, 'running', retry_at=NOW + 600)
    add_run(conn, cid, status='crashed')
    live = add_run(conn, cid)
    assert place(conn, cid) == ('waiting', 'external', 'retry', NOW + 600)
    assert place(conn, cid, probe(live)) == IN_PROGRESS


def test_running_card_past_its_retry_time_waits_for_a_slot(env):
    _, conn, _ = env
    cid = mk(conn, 'retry due')
    set_state(conn, cid, 'running', retry_at=NOW - 1)
    assert place(conn, cid) == SLOT


def test_review_is_in_progress_only_while_its_reviewer_lives(env):
    _, conn, _ = env
    cid = mk(conn, 'reviewed', review=True)
    set_state(conn, cid, 'review')
    add_run(conn, cid, status='exited')
    assert place(conn, cid) == SLOT
    reviewer = add_run(conn, cid, role='reviewer')
    assert place(conn, cid, probe(reviewer)) == IN_PROGRESS


def test_check_is_in_progress_while_its_gate_job_runs(env):
    _, conn, _ = env
    cid = mk(conn, 'gated')
    set_state(conn, cid, 'check')
    assert place(conn, cid) == SLOT
    conn.execute("INSERT INTO gate_jobs VALUES(?,1,'gate',?)", (cid, NOW - 5))
    assert place(conn, cid) == IN_PROGRESS


def test_real_probe_counts_a_missing_unit_as_gone_and_refuses_without_the_user_manager(env, tmp_path):
    _, conn, _ = env
    cid = mk(conn, 'unit gone')
    set_state(conn, cid, 'running')
    add_run(conn, cid, unit=UNIT)
    with pytest.raises(FileNotFoundError, match='user manager'):
        api.board(conn, NOW, cgroup_root=tmp_path)
    manager(tmp_path)
    [entry] = api.board(conn, NOW, cgroup_root=tmp_path)['columns']['waiting']
    assert (entry['id'], entry['badge']) == (cid, 'slot')


def dependency(conn, d):
    blocker = mk(conn, 'blocker')
    return mk(conn, 'dependent', blocked_by=[blocker]), dict(badge='card', cause='dependency', cards=[blocker])


def children(conn, d):
    epic = mk_epic(conn, 'epic')
    child = mk(conn, 'child', parent=epic)
    return epic, dict(badge='card', cause='children', cards=[child])


def hold(conn, d):
    return mk(conn, 'held', not_before=NOW + 3600), dict(badge='external', cause='hold', until=NOW + 3600)


def executor_wait(conn, d):
    cid = mk(conn, 'assigned')
    set_state(conn, cid, 'queued')
    d.wait_for_executor(api.card_row(conn, cid), 'vendor-a', 'weekly limit reached')
    return cid, dict(badge='external', cause='executor', reason='vendor-a: weekly limit reached')


def retry(conn, d):
    cid = mk(conn, 'backing off')
    set_state(conn, cid, 'queued', retry_at=NOW + 600)
    return cid, dict(badge='external', cause='retry', until=NOW + 600)


def failure(conn, d):
    cid = mk(conn, 'failed')
    d.finish(api.card_row(conn, cid), 'blocked', 'gate failed twice')
    return cid, dict(badge='external', cause='failure', reason='gate failed twice')


def stuck(conn, d):
    cid = mk(conn, 'head keeps dying')
    d.finish(api.card_row(conn, cid), 'blocked', 'stuck: 2 rounds in a row ended without a result',
             wait_reason=api.WAIT_STUCK)
    return cid, dict(badge='external', cause='stuck', reason='stuck: 2 rounds in a row ended without a result')


def created(conn, d):
    return mk(conn, 'new'), dict(badge='slot', cause='queue')


def hold_passed(conn, d):
    return mk(conn, 'hold passed', not_before=NOW - 1), dict(badge='slot', cause='queue')


def queued(conn, d):
    cid = mk(conn, 'queued')
    set_state(conn, cid, 'queued')
    return cid, dict(badge='slot', cause='queue')


def retry_due(conn, d):
    cid = mk(conn, 'retry due')
    set_state(conn, cid, 'queued', retry_at=NOW - 1)
    return cid, dict(badge='slot', cause='queue')


@pytest.mark.parametrize('setup', [dependency, children, hold, executor_wait, retry, failure, stuck, created,
                                   hold_passed, queued, retry_due])
def test_waiting_card_has_one_badge_and_its_reason(env, setup):
    _, conn, d = env
    cid, expected = setup(conn, d)
    [(column, entry)] = places(conn, cid)
    assert column == 'waiting'
    assert {k: entry[k] for k in ('badge', 'cause', 'reason', 'cards', 'until')} == {
        'reason': None, 'cards': [], 'until': None, **expected}


@pytest.mark.parametrize('age,place', [(api.DONE_WINDOW_S - 1, 'done'), (api.DONE_WINDOW_S + 1, 'archive')])
def test_done_window_edge(env, age, place):
    _, conn, _ = env
    cid = mk(conn, 'finished')
    set_state(conn, cid, 'done', finished_at=NOW - age)
    [(found, entry)] = places(conn, cid)
    assert found == place and entry['finished_at'] == NOW - age


def test_archive_holds_old_done_and_cancelled_newest_first_up_to_the_limit(env):
    _, conn, d = env
    old = mk(conn, 'old done')
    set_state(conn, old, 'done', finished_at=NOW - 10 * DAY)
    older = mk(conn, 'older done')
    set_state(conn, older, 'done', finished_at=NOW - 20 * DAY)
    cancelled = mk(conn, 'cancelled')
    assert d.cancel(cancelled, 'obsolete', 'secretary')
    conn.execute('UPDATE cards SET finished_at=? WHERE id=?', (NOW - 60, cancelled))
    recent = mk(conn, 'recent done')
    set_state(conn, recent, 'done', finished_at=NOW - 60)
    b = api.board(conn, NOW, archive_limit=2)
    assert [e['id'] for e in b['archive']] == [cancelled, old]
    assert [e['id'] for e in b['columns']['done']] == [recent]
    assert [e['id'] for e in api.board(conn, NOW, archive_limit=5)['archive']] == [cancelled, old, older]
    assert api.board(conn, NOW)['archive'] == []


def test_schedules_and_series_stay_off_the_board_and_their_followups_on_it(env, monkeypatch, capsys):
    cfg, conn, d = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    rule = mk_series(conn, every=60, max_runs=2)
    d.advance_series(d.clock())
    instance = only_instance(conn, rule)
    unlinked_run = mk(conn, 'run of an outside schedule', card_class='series_run')
    found_bug = mk(conn, 'found bug', followup_of=instance)
    research = mk(conn, 'extra research', followup_of=instance, card_class='followup')
    assert api.get(conn, found_bug)['card_class'] == 'series_run'
    for cid in (rule, instance, unlinked_run):
        assert places(conn, cid) == []
    set_state(conn, instance, 'done', finished_at=NOW - 30 * DAY)
    assert places(conn, instance) == []
    for cid in (found_bug, research):
        [(column, entry)] = places(conn, cid)
        assert column == 'waiting' and entry['followup_of'] == instance
    assert cli.main(['show', rule, '--json']) == 0
    assert [i['id'] for i in json.loads(capsys.readouterr().out)['instances']] == [instance]


def test_cli_board_json_archive_and_text(env, monkeypatch, capsys):
    cfg, conn, _ = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    running = mk(conn, 'running')
    set_state(conn, running, 'running')
    add_run(conn, running, pid=os.getpid())
    waiting = mk(conn, 'waiting', not_before=NOW + 60)
    done = mk(conn, 'done')
    set_state(conn, done, 'done', finished_at=NOW - DAY)
    archived = mk(conn, 'archived')
    set_state(conn, archived, 'done', finished_at=NOW - 3 * DAY)
    older = mk(conn, 'older')
    set_state(conn, older, 'done', finished_at=NOW - 4 * DAY)
    monkeypatch.setattr(cli.time, 'time', lambda: NOW)
    assert cli.main(['board', '--json']) == 0
    b = json.loads(capsys.readouterr().out)
    assert b['now'] == NOW and b['archive'] == []
    assert {name: [e['id'] for e in entries] for name, entries in b['columns'].items()} == {
        'in_progress': [running], 'waiting': [waiting], 'done': [done]}
    assert (b['columns']['waiting'][0]['badge'], b['columns']['waiting'][0]['until']) == ('external', NOW + 60)
    assert cli.main(['board', '--json', '--archive', '--limit', '1']) == 0
    assert [e['id'] for e in json.loads(capsys.readouterr().out)['archive']] == [archived]
    assert cli.main(['board', '--archive']) == 0
    text = capsys.readouterr().out
    assert all(cid in text for cid in (running, waiting, done, archived, older))
    with pytest.raises(SystemExit):
        cli.main(['board', '--archive', '--limit', '0'])


def test_list_json_keeps_its_rows_and_keys_beside_the_board(env, monkeypatch, capsys):
    cfg, conn, d = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    rule = mk_series(conn, every=60, max_runs=2)
    d.advance_series(d.clock())
    instance = only_instance(conn, rule)
    task = mk(conn, 'task')
    running = mk(conn, 'running')
    set_state(conn, running, 'running')
    add_run(conn, running, pid=os.getpid())
    assert cli.main(['list', '--json']) == 0
    before = capsys.readouterr().out
    assert cli.main(['board', '--json']) == 0
    capsys.readouterr()
    assert cli.main(['list', '--json']) == 0
    assert capsys.readouterr().out == before
    rows = json.loads(before)
    assert {r['id'] for r in rows} == {rule, instance, task, running}
    assert all(set(r) == LIST_KEYS for r in rows)


def test_unmapped_combination_is_an_error(env):
    _, conn, _ = env
    cid = mk(conn, 'odd')
    set_state(conn, cid, 'blocked', wait_reason='schedule')
    with pytest.raises(ValueError, match=cid):
        api.board(conn, NOW)
