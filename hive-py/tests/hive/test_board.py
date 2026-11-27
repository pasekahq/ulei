"""Board projection: columns, badges, the done window, the archive and the cards kept off the board."""
import json
import os

import pytest
from test_hive import env, mk, mk_epic, mk_series  # noqa: F401
from test_liveness import UNIT, manager
from hive import api, store

NOW = 2_000_000_000.0
DAY = 86400
LIST_KEYS = {'id', 'kind', 'card_class', 'owner_visible', 'urgent', 'state', 'wait_reason', 'step', 'review', 'round', 'title',
             'created_by', 'source', 'assignee', 'executor', 'on_unavailable', 'model', 'effort', 'panel', 'tag',
             'created_at', 'updated_at', 'parent', 'series', 'series_period', 'admission_class'}


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


def add_run(conn, cid, role='worker', status='running', pid=None, unit=None, vendor='fake'):
    return conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,pid,unit,started_at)
                           VALUES(?,1,?,?,'','',1,'',?,?,?,?)""",
                        (cid, role, vendor, status, pid, unit, NOW - 60)).lastrowid


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


def test_running_card_with_an_external_executor_job_waits_on_the_executor_unprobed(env):
    _, conn, _ = env
    cid = mk(conn, 'sent to an executor')
    set_state(conn, cid, 'running', executor='vendor-a')
    add_run(conn, cid, vendor='vendor-a')
    probed = []
    [(column, entry)] = places(conn, cid, alive=lambda run, cgroup_root: probed.append(run['id']))
    assert (column, entry['badge'], entry['cause'], entry['reason']) == ('waiting', 'external', 'executor', 'vendor-a')
    assert probed == []


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
    assert cli.span(NOW - b['columns']['waiting'][0]['waiting_since']) in text and '[external] hold for ' in text
    assert [cli.span(s) for s in (5, 60, 7200, 3 * DAY)] == ['5s', '1m', '2h', '3d']
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


def aged(conn, cid, state, wait_reason=None, created=NOW - DAY, updated=NOW - 1, priority=0):
    """Put the card in `state` as if created a day before NOW and last updated just before it."""
    set_state(conn, cid, state, wait_reason=wait_reason, created_at=created, updated_at=updated, priority=priority)
    return cid


def waiting_ids(conn):
    return [e['id'] for e in api.board(conn, NOW)['columns']['waiting']]


def since(conn, cid):
    [(column, entry)] = places(conn, cid)
    assert column == 'waiting'
    return entry['waiting_since']


def test_waiting_column_is_longest_waiting_first_not_by_priority(env):
    _, conn, _ = env
    urgent = aged(conn, mk(conn, 'urgent, blocked a minute ago'), 'blocked', priority=100)
    store.event(conn, urgent, 'blocked', {'reason': 'x'}, at=NOW - 60)
    old = aged(conn, mk(conn, 'background, blocked for hours'), 'blocked', priority=0)
    store.event(conn, old, 'blocked', {'reason': 'x'}, at=NOW - 5 * 3600)
    same_a = aged(conn, mk(conn, 'tie, created later'), 'blocked', created=NOW - DAY + 10)
    same_b = aged(conn, mk(conn, 'tie, created earlier'), 'blocked', created=NOW - DAY)
    for cid in (same_a, same_b):
        store.event(conn, cid, 'blocked', {'reason': 'x'}, at=NOW - 600)
    assert waiting_ids(conn) == [old, same_b, same_a, urgent]


# (state, wait_reason, the event that put it there): one per waiting_since row of the Board section.
ENTRIES = [('created', None, 'dependency_cleared'), ('queued', None, 'retry'), ('queued', None, 'process_ended'),
           ('running', None, 'run_reaped'), ('review', None, 'review_requested'), ('check', None, 'gate'),
           ('blocked', None, 'blocked'), ('blocked', api.WAIT_JUDGE, 'blocked'),
           ('blocked', api.WAIT_PROCESS, 'process_wait'), ('blocked', api.WAIT_DEPENDENCY, 'dependency_wait')]


@pytest.mark.parametrize('state,wait_reason,kind', ENTRIES)
def test_waiting_since_is_the_latest_transition_into_the_current_state(env, state, wait_reason, kind):
    _, conn, _ = env
    cid = mk(conn, 'waits')
    if wait_reason == api.WAIT_DEPENDENCY:
        blocker = mk(conn, 'blocker')
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'blocked_by',?,?)", (cid, blocker, NOW))
    aged(conn, cid, state, wait_reason)
    store.event(conn, cid, kind, None, at=NOW - 7200)  # an earlier stay in the same state
    store.event(conn, cid, 'claimed' if state not in ('running', 'review') else 'steer', None, at=NOW - 5000)
    store.event(conn, cid, kind, None, at=NOW - 3600)
    store.event(conn, cid, 'steer', {'text': 'later note'}, at=NOW - 60)
    store.event(conn, cid, 'tagged', {'tag': 't'}, at=NOW - 30)
    assert since(conn, cid) == NOW - 3600


def test_waiting_since_of_an_epic_is_its_children_wait(env):
    _, conn, _ = env
    epic = mk_epic(conn, 'epic')
    mk(conn, 'child', parent=epic)
    [row] = conn.execute("SELECT at FROM events WHERE card_id=? AND kind='children_wait'", (epic,)).fetchall()
    store.event(conn, epic, 'child_added', {}, at=row['at'] + 5)
    assert since(conn, epic) == row['at']


def test_waiting_since_falls_back_to_updated_at_and_stays_within_created_at_and_now(env):
    _, conn, _ = env
    plain = aged(conn, mk(conn, 'queued without a queued event'), 'queued', updated=NOW - 900)
    store.event(conn, plain, 'steer', {'text': 'not a transition'}, at=NOW - 60)
    assert since(conn, plain) == NOW - 900
    early = aged(conn, mk(conn, 'event before the card'), 'blocked', created=NOW - 100)
    store.event(conn, early, 'blocked', None, at=NOW - 200)
    assert since(conn, early) == NOW - 100
    late = aged(conn, mk(conn, 'event after now'), 'blocked')
    store.event(conn, late, 'blocked', None, at=NOW + 50)
    assert since(conn, late) == NOW


def test_in_progress_stays_in_priority_order(env):
    _, conn, _ = env
    low = aged(conn, mk(conn, 'low, older'), 'running', created=NOW - DAY, priority=0)
    high = aged(conn, mk(conn, 'high, newer'), 'running', created=NOW - 60, priority=100)
    live = [add_run(conn, cid) for cid in (low, high)]
    b = api.board(conn, NOW, alive=probe(*live))
    assert [e['id'] for e in b['columns']['in_progress']] == [high, low]
    assert all('waiting_since' not in e for e in b['columns']['in_progress'])


def test_unmapped_combination_is_an_error(env):
    _, conn, _ = env
    cid = mk(conn, 'odd')
    set_state(conn, cid, 'blocked', wait_reason='schedule')
    with pytest.raises(ValueError, match=cid):
        api.board(conn, NOW)
