"""Acceptance exit 75 is «do not know»: the gate and the recheck keep the card where it is and ask again later."""
import pytest
from test_hive import completed_scheduled_card, env, fake_time, kinds, mk_epic  # noqa: F401
from hive import api, dispatcher

UNKNOWN = 'printf "ssh: connect to host: Connection refused" >&2; exit 75'


def mk(conn, title, **kw):
    return api.create(conn, title=title, goal='g', step=1, source='system:test', created_by='secretary', **kw)[0]


def outbox(conn, cid):
    return [r['kind'] for r in conn.execute('SELECT kind FROM outbox WHERE card_id=?', (cid,))]


def row(conn, cid):
    return conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()


def in_check(conn, cid, tmp_path):
    conn.execute("UPDATE cards SET state='check', round=1, workdir=?, version=version+1 WHERE id=?", (str(tmp_path), cid))
    return row(conn, cid)


def test_unknown_recheck_keeps_the_card_done_and_moves_the_due_without_an_attempt(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    cid = completed_scheduled_card(conn, tmp_path, acceptance=UNKNOWN, recheck_every=60, recheck_max=2)

    now[0] = 1060
    assert d.advance() == 1
    card = api.get(conn, cid)
    assert card['state'] == 'done' and card['block_reason'] is None
    assert card['recheck_state'] == {'due': 1120.0, 'attempts': 0}
    assert card['gates'] == []
    event = card['events'][-1]
    assert event['kind'] == 'gate_unknown'
    assert event['data']['recheck_attempt'] == 1 and event['data']['exit'] == 75
    assert event['data']['recheck_due'] == 1120.0 and event['data']['unknowns'] == 1
    assert 'Connection refused' in event['data']['output']
    assert 'blocked' not in kinds(conn, cid) and 'recheck' not in kinds(conn, cid)
    assert outbox(conn, cid) == []

    # The system answers again: the same attempt is decided.
    conn.execute("UPDATE cards SET acceptance='true' WHERE id=?", (cid,))
    now[0] = 1120
    assert d.advance() == 1
    card = api.get(conn, cid)
    assert card['state'] == 'done'
    assert card['recheck_state'] == {'due': 1180.0, 'attempts': 1}
    assert card['events'][-1]['data'] == {'attempt': 1, 'exit': 0, 'passed': True, 'next_due': 1180.0}


def test_unknown_recheck_whose_next_ask_is_past_the_deadline_is_a_failure(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    cid = completed_scheduled_card(conn, tmp_path, acceptance=UNKNOWN, recheck_every=60, recheck_until=1120)

    now[0] = 1060
    assert d.advance() == 1
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and card['block_reason'] == 'recheck exit 75'
    assert 'gate_unknown' not in kinds(conn, cid)
    assert outbox(conn, cid) == ['task_blocked']


def test_unknown_recheck_stops_at_unknown_max_in_a_row(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    monkeypatch.setattr(dispatcher, 'UNKNOWN_MAX', 3)
    cid = completed_scheduled_card(conn, tmp_path, acceptance=UNKNOWN, recheck_every=60, recheck_max=1)
    for due in (1060, 1120):
        now[0] = due
        assert d.recheck(row(conn, cid)) == 1
        assert row(conn, cid)['state'] == 'done'
    now[0] = 1180
    assert d.recheck(row(conn, cid)) == 1
    assert row(conn, cid)['state'] == 'blocked'
    assert kinds(conn, cid).count('gate_unknown') == 2


@pytest.mark.parametrize('kind', ['epic', 'task'])
def test_unknown_gate_keeps_the_card_in_check_and_asks_again_after_the_retry(env, monkeypatch, tmp_path, kind):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    cid = (mk_epic(conn, 'waits on a board', acceptance=UNKNOWN) if kind == 'epic'
           else mk(conn, 'waits on a board', acceptance=UNKNOWN))
    assert d.gate(in_check(conn, cid, tmp_path)) == 1

    c = row(conn, cid)
    assert c['state'] == 'check' and c['retry_at'] == 1000 + dispatcher.UNKNOWN_RETRY_S
    assert c['block_reason'] is None
    assert conn.execute('SELECT count(*) FROM gates WHERE card_id=?', (cid,)).fetchone()[0] == 0
    k = kinds(conn, cid)
    assert k[-1] == 'gate_unknown' and not {'gate', 'retry', 'blocked', 'loop_stopped'} & set(k)
    assert outbox(conn, cid) == []

    scheduled = []
    monkeypatch.setattr(d, 'schedule_gate', lambda c, mode: scheduled.append((c['id'], mode)) or 1)
    now[0] = 1000 + dispatcher.UNKNOWN_RETRY_S - 1
    d.schedule_checks()
    assert scheduled == []
    now[0] = 1000 + dispatcher.UNKNOWN_RETRY_S
    d.schedule_checks()
    assert scheduled == [(cid, 'gate')]


def test_unknown_gate_of_a_waiting_card_asks_again_after_recheck_every(env, monkeypatch, tmp_path):
    _, conn, d = env
    fake_time(monkeypatch, d)
    cid = mk(conn, 'waits on a sprint', acceptance=UNKNOWN, recheck_every=600, recheck_max=5)
    assert d.gate(in_check(conn, cid, tmp_path)) == 1
    c = row(conn, cid)
    assert c['state'] == 'check' and c['retry_at'] == 1600
    data = api.get(conn, cid)['events'][-1]['data']
    assert (data['round'], data['exit'], data['retry_at'], data['unknowns']) == (1, 75, 1600, 1)


def test_unknown_gate_at_unknown_max_in_a_row_fails_the_round(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    monkeypatch.setattr(dispatcher, 'UNKNOWN_MAX', 2)
    cid = mk_epic(conn, 'waits on a board', acceptance=UNKNOWN)
    assert d.gate(in_check(conn, cid, tmp_path)) == 1
    now[0] += dispatcher.UNKNOWN_RETRY_S
    assert d.gate(row(conn, cid)) == 1
    c = api.get(conn, cid)
    assert c['state'] == 'blocked' and c['block_reason'] == 'gate exit 75'
    assert c['gates'][-1]['exit_code'] == 75


def test_other_failing_exits_are_still_failures(env, monkeypatch, tmp_path):
    _, conn, d = env
    fake_time(monkeypatch, d)
    cid = mk_epic(conn, 'board answered no', acceptance='exit 1')
    assert d.gate(in_check(conn, cid, tmp_path)) == 1
    assert row(conn, cid)['state'] == 'blocked'
    assert 'gate_unknown' not in kinds(conn, cid)
