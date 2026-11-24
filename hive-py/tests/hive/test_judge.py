"""The one-shot judge against the scripted qb gateway: verdicts, the route of the card's step, retries at the gateway's
time, waits naming the class, and one turn per judgement."""
import time

import pytest
from fake_gateway import C4G, TURN_REQUEST, answer, fixture, prompt_of, rejected, stream
from schema_check import violations
from test_hive import env, record_spawns  # noqa: F401
from hive import api, c4g

REPORT = 'RESULT: done, the totals add up.\nevidence: totals.csv'
RESETS_AT = fixture('StreamEvent.error_quota_exhausted')['error']['resets_at']


def in_check(conn, step=1, acceptance='echo gate-says-ok', review=None, **kw):
    """A head card whose round-1 worker exited with REPORT, in check before its gate; with `review` = (verdict, text)
    its gate passed and it is in review with that reviewer run finished but not yet handled."""
    cid, _ = api.create(conn, title='sum the ledger', goal='write the totals', acceptance=acceptance, step=step,
                        source='system:test', created_by='secretary', review=review is not None, **kw)
    now = time.time()
    run = """INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,handled,started_at,ended_at,
             exit_code,verdict,report) VALUES(?,1,?,'fake','fake','fake',?,'prompt','exited',?,?,?,0,?,?)"""
    conn.execute(run, (cid, 'worker', step, 1, now, now, None, REPORT))
    if review is not None:
        conn.execute('INSERT INTO gates(card_id,round,command,exit_code,passed,output,at) VALUES(?,1,?,0,1,?,?)',
                     (cid, acceptance, 'gate-says-ok\n', now))
        conn.execute(run, (cid, 'reviewer', step, 0, now, now, *review))
    conn.execute('UPDATE cards SET state=?, round=1, version=version+1 WHERE id=?',
                 ('check' if review is None else 'review', cid))
    return cid


def settle(d):
    """Dispatcher passes enough for a gate and then a judge; the env runs both inline."""
    for _ in range(3):
        d.advance()


def waiting(conn, cid, now):
    board = api.board(conn, now, alive=lambda run, root: False)
    [entry] = [e for e in board['columns']['waiting'] if e['id'] == cid]
    return {k: entry[k] for k in ('badge', 'cause', 'reason', 'until')}


def test_done_finishes_the_card_after_one_turn_holding_the_task_report_and_gate(env, gateway):
    _, conn, d = env
    cid = in_check(conn, body='use the march ledger', acceptance_kind='judge', judge_criteria='names its evidence')
    settle(d)
    card = api.get(conn, cid)
    assert (card['state'], card['result']) == ('done', 'judge done')
    [request] = gateway.requests
    text = prompt_of(request)
    for part in ('sum the ledger', 'write the totals', 'use the march ledger', 'echo gate-says-ok', 'names its evidence',
                 REPORT, 'exit 0\ngate-says-ok'):
        assert part in text
    assert request['max_tokens'] == 1024
    assert [e['data'] for e in card['events'] if e['kind'] == 'judge'] == [
        {'round': 1, 'route': 'judge-s1', 'verdict': 'done', 'reason': ''}]


def test_rework_starts_a_new_round_with_the_reason_as_feedback(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: rework\nThe March totals are missing.')]
    cid = in_check(conn)
    settle(d)
    card = api.get(conn, cid)
    assert (card['state'], card['feedback']) == ('queued', 'The March totals are missing.')
    assert [e['data']['reason'] for e in card['events'] if e['kind'] == 'retry'] == [
        'judge rework: The March totals are missing.']
    spawned = record_spawns(d)
    d.launch()
    [prompt] = [r['prompt'] for r in api.get(conn, cid)['runs'] if r['round'] == 2]
    assert len(spawned) == 1 and 'The March totals are missing.' in prompt
    assert len(gateway.requests) == 1


def test_failed_gate_reworks_without_a_turn(env, gateway):
    _, conn, d = env
    cid = in_check(conn, acceptance='echo totals differ; false')
    settle(d)
    card = api.get(conn, cid)
    assert card['state'] == 'queued' and 'totals differ' in card['feedback']
    assert gateway.requests == []


def test_review_fail_text_reaches_the_prompt_and_the_judge_decides(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: done')]
    cid = in_check(conn, review=('FAIL', 'VERDICT: FAIL\nthe April column is empty\n'))
    settle(d)
    [request] = gateway.requests
    assert '## Review: VERDICT FAIL\nVERDICT: FAIL\nthe April column is empty' in prompt_of(request)
    assert api.card_row(conn, cid)['state'] == 'done'


@pytest.mark.parametrize('step', [1, 3, 5])
def test_route_is_the_alias_of_the_card_step(env, gateway, step):
    _, conn, d = env
    cid = in_check(conn, step=step)
    settle(d)
    assert [r['route'] for r in gateway.requests] == [f'judge-s{step}']
    assert api.card_row(conn, cid)['state'] == 'done'


START = RESETS_AT - 1000.0


@pytest.mark.parametrize('failure,due', [
    (stream(fixture('StreamEvent.error_quota_exhausted')), RESETS_AT),
    (rejected(429, {'class': 'overloaded', 'retryable': True, 'retry_after': 30, 'message': 'turn cap reached'}),
     START + 30),
    (stream(fixture('StreamEvent.route'), fixture('StreamEvent.text_delta')), START + 300),
], ids=['quota_exhausted', 'overloaded', 'cut_stream'])
def test_retryable_failure_waits_for_the_gateway_time_then_judges_again(env, gateway, failure, due):
    _, conn, d = env
    now = [START]
    d.clock = lambda: now[0]
    gateway.script = [failure, answer('VERDICT: done')]
    cid = in_check(conn)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['retry_at']) == ('check', due)
    assert waiting(conn, cid, now[0]) == {'badge': 'external', 'cause': 'retry', 'reason': None, 'until': due}
    now[0] = due - 1
    settle(d)
    assert len(gateway.requests) == 1
    now[0] = due
    settle(d)
    assert api.card_row(conn, cid)['state'] == 'done' and len(gateway.requests) == 2


@pytest.mark.parametrize('failure,cls', [
    (rejected(404, fixture('Error.no_route')), 'no_route'),
    (stream(fixture('StreamEvent.route'), fixture('StreamEvent.error_bad_request')), 'bad_request'),
])
def test_non_retryable_failure_waits_naming_the_class(env, gateway, failure, cls):
    _, conn, d = env
    gateway.script = [failure]
    cid = in_check(conn)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'judge')
    assert card['block_reason'].startswith(f'judge {cls}: ')
    assert waiting(conn, cid, time.time()) == {'badge': 'external', 'cause': 'judge', 'reason': card['block_reason'],
                                               'until': None}
    assert len(gateway.requests) == 1


@pytest.mark.parametrize('text', ['The totals look right to me.', 'VERDICT: rework'])
def test_answer_without_a_verdict_or_a_reason_waits(env, gateway, text):
    _, conn, d = env
    gateway.script = [answer(text)]
    cid = in_check(conn)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'judge')
    assert card['block_reason'].startswith('judge unparseable: ')
    assert len(gateway.requests) == 1


def test_unreachable_gateway_is_a_retryable_failure_value(tmp_path):
    failure = c4g.turn(str(tmp_path / 'absent.sock'), c4g.request('judge-s1', 'x', 16, 'turn-1'), 5)
    assert (failure.cls, failure.retryable) == ('unavailable', True) and failure.message.startswith('transport: ')


@pytest.mark.parametrize('kind,name', [('valid', 'TurnRequest.all_blocks'), ('valid', 'TurnRequest.minimal'),
                                       ('invalid', 'TurnRequest.extra_field'), ('invalid', 'TurnRequest.missing_route')])
def test_fake_gateway_check_follows_the_vendored_turn_request_fixtures(kind, name):
    assert bool(violations(fixture(name, kind), TURN_REQUEST, C4G)) == (kind == 'invalid')
