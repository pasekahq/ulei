"""A dispute about the task itself: the worker's `RESULT: disputed` or the judge's `VERDICT: disputed` is no failure and
no rework round; the card waits for its author, who accepts the worker's result or starts a rework round."""
import json
import subprocess
import sys
import time
from pathlib import Path

import pytest
from fake_gateway import answer
from test_hive import env, record_spawns  # noqa: F401
from test_judge import in_check, settle, waiting
from hive import api, judge

DISPUTE = 'RESULT: disputed: the order is already right on the rendered page, a PR would change nothing.\nsteps: ...'
ROOT = Path(__file__).resolve().parents[2]


def worker_says(conn, cid, report):
    conn.execute("UPDATE runs SET report=? WHERE card_id=? AND role='worker'", (report, cid))


def outbox(conn, cid):
    return {r['event_id']: json.loads(r['payload']) for r in
            conn.execute('SELECT event_id, payload FROM outbox WHERE card_id=? ORDER BY rowid', (cid,))}


@pytest.mark.parametrize('text,found', [
    ('VERDICT: disputed\nThe card asks for a PR the reproduction shows is not needed.',
     ('disputed', 'The card asks for a PR the reproduction shows is not needed.')),
    ('**VERDICT: Disputed** the premise is wrong', ('disputed', 'the premise is wrong')),
    ('VERDICT: disputed', None),
])
def test_verdict_reads_a_dispute_with_its_reason(text, found):
    assert judge.verdict(text) == found


def test_judge_dispute_waits_for_the_author_without_a_failure(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: disputed\nThe worker shows the order is right; the card demands a PR anyway.')]
    cid = in_check(conn)
    settle(d)
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'disputed')
    assert card['block_reason'] == 'disputed by the judge: The worker shows the order is right; the card demands a PR anyway.'
    assert [e for e in card['events'] if e['kind'] == 'retry'] == []
    assert waiting(conn, cid, time.time())['cause'] == 'disputed'
    [(event_id, payload)] = outbox(conn, cid).items()
    assert event_id == f'hive:{cid}:disputed:1'
    assert payload['disputed'] == {'round': 1, 'by': 'judge', 'author': 'secretary',
                                   'reason': 'The worker shows the order is right; the card demands a PR anyway.'}
    assert 'resolve' in payload['note'] and payload['next_actor'] == 'secretary'


def test_judge_rework_of_a_disputing_worker_is_a_dispute(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: rework\nNo PR link.')]
    cid = in_check(conn)
    worker_says(conn, cid, DISPUTE)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason'], card['block_reason']) == (
        'blocked', 'disputed', 'disputed by the judge: No PR link.')


def test_judge_may_still_accept_a_disputing_worker_by_its_result(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: done')]
    cid = in_check(conn)
    worker_says(conn, cid, DISPUTE)
    settle(d)
    assert api.card_row(conn, cid)['state'] == 'done'


def test_failed_gate_of_a_disputing_worker_goes_to_the_author_without_a_turn(env, gateway):
    _, conn, d = env
    cid = in_check(conn, acceptance='echo no PR found; false')
    worker_says(conn, cid, DISPUTE)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'disputed')
    assert card['block_reason'].startswith('disputed by the worker: RESULT: disputed: the order is already right')
    assert 'no PR found' in card['feedback'] and gateway.requests == []


def test_author_accepts_the_worker_result(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: disputed\nartifact not needed')]
    cid = in_check(conn)
    settle(d)
    assert d.resolve(cid, 'accept', 'the reproduction is enough') == 'done'
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason'], card['block_reason']) == ('done', None, None)
    assert card['result'] == 'dispute resolved by secretary: accepted: the reproduction is enough'
    [resolved] = [e['data'] for e in card['events'] if e['kind'] == 'dispute_resolved']
    assert resolved == {'round': 1, 'decision': 'accept', 'text': 'the reproduction is enough', 'by': 'secretary'}
    assert list(outbox(conn, cid)) == [f'hive:{cid}:disputed:1', f'hive:{cid}:done']


def test_author_rework_starts_a_round_with_the_instructions_and_spends_no_failure(env, gateway):
    _, conn, d = env
    gateway.script = [answer('VERDICT: disputed\nartifact not needed'),
                      answer('VERDICT: disputed\nstill disputed')]
    cid = in_check(conn)
    settle(d)
    assert d.resolve(cid, 'rework', 'The owner sees the bug: reproduce it on the page first.') == 'queued'
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason'], card['finished_at']) == ('queued', None, None)
    spawned = record_spawns(d)
    d.launch()
    [prompt] = [r['prompt'] for r in api.get(conn, cid)['runs'] if r['round'] == 2]
    assert len(spawned) == 1 and 'reproduce it on the page first' in prompt
    assert conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='retry'", (cid,)).fetchone()[0] == 0


def test_resolve_refuses_a_card_that_is_not_disputed_and_a_rework_without_text(env, gateway):
    _, conn, d = env
    cid = in_check(conn)
    with pytest.raises(api.CardError, match='not disputed'):
        d.resolve(cid, 'accept')
    gateway.script = [answer('VERDICT: disputed\nx')]
    settle(d)
    with pytest.raises(api.CardError, match='instructions'):
        d.resolve(cid, 'rework', '  ')
    with pytest.raises(api.CardError, match="card's author"):
        d.resolve(cid, 'accept', actor='someone-else')
    with pytest.raises(api.CardError, match='resolve'):
        api.release(conn, cid)


def test_cli_resolve(env, gateway):
    cfg, conn, d = env
    gateway.script = [answer('VERDICT: disputed\nartifact not needed')]
    cid = in_check(conn)
    settle(d)
    out = subprocess.run([sys.executable, str(ROOT / 'scripts' / 'hive.py'), 'resolve', cid, 'accept', 'ok'],
                         capture_output=True, text=True)
    assert out.returncode == 0, out.stderr
    assert out.stdout.strip() == f'{cid} dispute resolved: accept, now done'
    assert api.card_row(conn, cid)['state'] == 'done'


def test_a_pending_clarification_gets_its_round_before_a_worker_dispute(env, gateway):
    _, conn, d = env
    cid = in_check(conn, acceptance='echo no PR found; false')
    worker_says(conn, cid, DISPUTE)
    api.steer(conn, cid, 'The owner sees the wrong order on the page.', actor='secretary')
    settle(d)
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('queued', None)
    assert [e for e in card['events'] if e['kind'] in ('disputed', 'retry')] == []
    assert [e['kind'] for e in card['events']][-1] == 'steer_round'


def test_inspect_shows_a_disputed_round_as_no_failure(env, gateway):
    from hive import inspect
    _, conn, d = env
    gateway.script = [answer('VERDICT: disputed\nartifact not needed')]
    cid = in_check(conn)
    settle(d)
    [entry] = inspect.rounds(conn, cid)
    assert entry['verdict'] == {'result': 'DISPUTED', 'by': 'judge', 'detail': 'the card author decides'}
    assert [e['kind'] for e in entry['events']][:1] == ['disputed']
