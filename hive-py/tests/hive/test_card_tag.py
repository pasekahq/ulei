"""The user tag of a card: set at create, set or cleared while not final, shown in show, list and the board, carried by
the judge's C4G request, and never dropped or bypassed when qb refuses the tagged request."""
import json

import pytest
from fake_gateway import answer, fixture, no_login, rejected
from test_board import set_state
from test_hive import env, mk, mk_series  # noqa: F401
from test_judge import START, in_check, settle, waiting
from hive import api

TAG = 'ledger'


def cli_json(capsys, cli, *argv):
    assert cli.main([*argv, '--json']) == 0
    return json.loads(capsys.readouterr().out)


def test_tag_set_at_create_shows_in_show_list_and_the_board(env, monkeypatch, capsys):
    cfg, conn, _ = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    created = cli_json(capsys, cli, 'create', '--title', 'sum the ledger', '--goal', 'write the totals',
                       '--acceptance', 'true', '--step', '1', '--source', 'system:test', '--created-by', 'secretary',
                       '--tag', TAG)
    untagged = mk(conn, 'untagged')
    assert cli_json(capsys, cli, 'show', created['id'])['tag'] == TAG
    assert {r['id']: r['tag'] for r in cli_json(capsys, cli, 'list')} == {created['id']: TAG, untagged: None}
    board = cli_json(capsys, cli, 'board')
    assert {e['id']: e['tag'] for e in board['columns']['waiting']} == {created['id']: TAG, untagged: None}


def test_tag_is_set_changed_and_cleared_on_a_card_that_is_not_final(env, monkeypatch, capsys):
    cfg, conn, _ = env
    from scripts import hive as cli
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    cid = mk(conn, 'tag me')
    for argv, tag in ((['tag', cid, TAG], TAG), (['tag', cid, 'travel'], 'travel'), (['tag', cid, '--none'], None)):
        assert cli.main(argv) == 0
        assert api.card_row(conn, cid)['tag'] == tag
    capsys.readouterr()
    assert cli.main(['tag', cid, '--none']) == 0
    assert 'unchanged' in capsys.readouterr().out
    assert [(e['data']['tag'], e['data']['previous']) for e in api.get(conn, cid)['events'] if e['kind'] == 'tagged'] == [
        (TAG, None), ('travel', TAG), (None, 'travel')]
    set_state(conn, cid, 'done')
    assert cli.main(['tag', cid, TAG]) == 2
    assert 'final card' in capsys.readouterr().err
    assert api.card_row(conn, cid)['tag'] is None


@pytest.mark.parametrize('tag', ['', 'Ledger', '1ledger', 'led ger', 'a' * 65, 7])
def test_invalid_tag_is_refused_at_create_and_set(env, tag):
    _, conn, _ = env
    with pytest.raises(api.CardError, match='tag must match'):
        mk(conn, 'bad tag', tag=tag)
    cid = mk(conn, 'good card')
    with pytest.raises(api.CardError, match='tag must match'):
        api.set_tag(conn, cid, tag)
    assert api.card_row(conn, cid)['tag'] is None


def test_series_instance_carries_the_tag_of_its_rule(env):
    _, conn, d = env
    rule = mk_series(conn, every=60, max_runs=1, tag=TAG)
    d.advance_series(d.clock())
    [instance] = api.get(conn, rule)['instances']
    assert api.card_row(conn, instance['id'])['tag'] == TAG


def test_tagged_card_sends_one_turn_with_the_tag_and_the_verdict_applies(env, gateway):
    _, conn, d = env
    cid = in_check(conn, tag=TAG)
    settle(d)
    [request] = gateway.requests
    assert (request['tag'], request['route']) == (TAG, 'judge-s1')
    card = api.get(conn, cid)
    assert (card['state'], card['result']) == ('done', 'judge done')
    assert [e['data'] for e in card['events'] if e['kind'] == 'judge'] == [
        {'round': 1, 'route': 'judge-s1', 'tag': TAG, 'verdict': 'done', 'reason': ''}]


def test_untagged_card_sends_the_request_without_a_tag_field(env, gateway):
    _, conn, d = env
    in_check(conn)
    settle(d)
    [request] = gateway.requests
    assert set(request) == {'route', 'messages', 'max_tokens', 'request_id', 'system'}


@pytest.mark.parametrize('failure,cls', [(no_login(TAG), 'no_login_for_tag'),
                                         (rejected(404, fixture('Error.no_route')), 'no_route')])
def test_non_retryable_refusal_of_a_tagged_request_waits_naming_tag_and_class(env, gateway, failure, cls):
    _, conn, d = env
    gateway.script = [failure]
    cid = in_check(conn, tag=TAG)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason'], card['tag']) == ('blocked', 'judge', TAG)
    assert card['block_reason'].startswith(f'judge {cls} for tag {TAG}: ')
    assert waiting(conn, cid, START) == {'badge': 'external', 'cause': 'judge', 'reason': card['block_reason'],
                                         'until': None}
    settle(d)
    assert [r['tag'] for r in gateway.requests] == [TAG]


def test_tag_unaware_gateway_refuses_with_bad_request_and_the_card_waits_keeping_the_tag(env, gateway):
    _, conn, d = env
    gateway.tags = False
    cid = in_check(conn, tag=TAG)
    settle(d)
    settle(d)
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason'], card['tag']) == ('blocked', 'judge', TAG)
    assert card['block_reason'] == f'judge bad_request for tag {TAG}: $: unexpected tag'
    assert [r['tag'] for r in gateway.requests] == [TAG]
    assert gateway.invalid == [['$: unexpected tag']]
    gateway.invalid.clear()


def test_retryable_failure_retries_only_the_same_tagged_request(env, gateway):
    _, conn, d = env
    now = [START]
    d.clock = lambda: now[0]
    gateway.script = [rejected(429, {'class': 'overloaded', 'retryable': True, 'retry_after': 30, 'message': 'busy'}),
                      answer('VERDICT: done')]
    cid = in_check(conn, tag=TAG)
    settle(d)
    assert waiting(conn, cid, now[0]) == {'badge': 'external', 'cause': 'retry', 'reason': f'tag {TAG}',
                                          'until': START + 30}
    now[0] = START + 30
    settle(d)
    assert api.card_row(conn, cid)['state'] == 'done'
    first, second = gateway.requests
    assert {**first, 'request_id': None} == {**second, 'request_id': None} and second['tag'] == TAG
