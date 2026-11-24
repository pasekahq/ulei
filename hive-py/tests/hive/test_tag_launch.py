"""A tagged card's heads: the tag reaches the configured launcher, heads run only on vendors whose binary is the tag-aware
launcher, and a launcher refusal parks the card naming the tag and the class with no other head run."""
import sys
import time

import pytest
from test_hive import EXECUTORS, FAKE, env, kinds, mk, run_heads_inline, tick_until_terminal  # noqa: F401
from test_judge import waiting
from hive import api, heads

TAG = 'ledger'
REFUSAL = 'no login bound to the tag'


def install_launchers(cfg, tmp_path, refuse=()):
    """Make each vendor's binary a fake launcher that logs its vendor and HIVE_TAG, then runs the fake head or, for a
    vendor in `refuse`, refuses the tag; returns a reader of the log as [(vendor, tag or None)]."""
    log = tmp_path / 'launches.log'
    for vendor in heads.VENDORS:
        script = tmp_path / 'bin' / f'launcher-{vendor}'
        run = (f"echo 'tag refused: no_login_for_tag: {REFUSAL}' >&2\nexit 77\n" if vendor in refuse
               else f'exec {sys.executable} {FAKE} "$@"\n')
        script.write_text(f'#!/bin/sh\nif [ "${{HIVE_TAG+set}}" = set ]; then t="$HIVE_TAG"; else t=-; fi\n'
                          f'echo "{vendor} $t" >> {log}\n{run}')
        script.chmod(0o755)
        cfg['vendors'][vendor]['binary'] = [str(script)]

    def launches():
        lines = log.read_text().split('\n')[:-1] if log.exists() else []
        return [(vendor, None if tag == '-' else tag) for vendor, tag in (line.split(' ') for line in lines)]
    return launches


def heads_of(conn, cid):
    return [(r['role'], r['vendor']) for r in api.get(conn, cid)['runs']]


def test_tagged_heads_get_the_tag_and_untagged_heads_do_not(env, tmp_path):
    cfg, conn, d = env
    cfg['vendors']['codex']['tag_launcher'] = True
    launches = install_launchers(cfg, tmp_path)
    run_heads_inline(cfg, d)
    tagged = mk(conn, '[ok] tagged', review=True, tag=TAG)
    assert tick_until_terminal(d, conn, tagged) == 'done'
    assert [role for role, _ in heads_of(conn, tagged)] == ['worker', 'reviewer']
    assert launches() == [(vendor, TAG) for _, vendor in heads_of(conn, tagged)]
    untagged = mk(conn, '[ok] untagged', review=True)
    assert tick_until_terminal(d, conn, untagged) == 'done'
    assert launches()[2:] == [(vendor, None) for _, vendor in heads_of(conn, untagged)]


@pytest.mark.parametrize('review,refuse', [(False, {'claude', 'codex'}), (True, {'codex'})], ids=['worker', 'reviewer'])
def test_launcher_refusal_waits_naming_tag_and_class_and_runs_no_other_head(env, tmp_path, review, refuse):
    cfg, conn, d = env
    cfg['vendors']['codex']['tag_launcher'] = True
    cfg['review_same_vendor_fallback'] = True
    launches = install_launchers(cfg, tmp_path, refuse)
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] refused', review=review, tag=TAG, assignee='claude', on_unavailable='fallback',
             executors=EXECUTORS)
    assert tick_until_terminal(d, conn, cid) == 'blocked'
    card = api.card_row(conn, cid)
    assert (card['state'], card['wait_reason'], card['tag']) == ('blocked', 'tag', TAG)
    assert card['block_reason'] == f'launcher no_login_for_tag for tag {TAG}: {REFUSAL}'
    assert waiting(conn, cid, time.time()) == {'badge': 'external', 'cause': 'tag', 'reason': card['block_reason'],
                                               'until': None}
    for _ in range(3):
        d.tick()
    expected = [('worker', 'claude'), ('reviewer', 'codex')] if review else [('worker', 'claude')]
    assert heads_of(conn, cid) == expected
    assert launches() == [(vendor, TAG) for _, vendor in expected]
    assert not {'provider_failover', 'retry', 'assignee_fallback'} & set(kinds(conn, cid))


def test_provider_failure_of_a_tagged_head_retries_only_on_a_tag_capable_vendor(env, tmp_path):
    cfg, conn, d = env
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 10}
    now = [1000.0]
    d.clock = lambda: now[0]
    launches = install_launchers(cfg, tmp_path)
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [provider429] tagged outage', tag=TAG)
    d.tick()
    d.tick()
    assert api.card_row(conn, cid)['state'] == 'queued'
    assert launches() == [('claude', TAG)]
    assert 'provider_failover' not in kinds(conn, cid)
    now[0] += 10
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert heads_of(conn, cid) == [('worker', 'claude'), ('worker', 'claude')]
    assert launches() == [('claude', TAG), ('claude', TAG)]


def test_tagged_heads_never_pick_a_vendor_without_the_launcher(env, tmp_path):
    cfg, conn, d = env
    cfg['review_same_vendor_fallback'] = True
    launches = install_launchers(cfg, tmp_path)
    run_heads_inline(cfg, d)
    tagged = [mk(conn, f'[ok] tagged {i}', review=True, tag=TAG) for i in range(3)]
    assigned = mk(conn, '[ok] assigned elsewhere', tag=TAG, assignee='codex', on_unavailable='fallback',
                  executors=EXECUTORS)
    untagged = mk(conn, '[ok] untagged', review=True)
    for cid in (*tagged, assigned, untagged):
        assert tick_until_terminal(d, conn, cid) == 'done'
    for cid in tagged:
        assert heads_of(conn, cid) == [('worker', 'claude'), ('reviewer', 'claude')]
    assert heads_of(conn, assigned) == [('worker', 'claude')]
    assert [e['data']['reason'] for e in api.get(conn, assigned)['events'] if e['kind'] == 'assignee_fallback'] == [
        f'no tag launcher for tag {TAG}']
    [(_, worker), (_, reviewer)] = heads_of(conn, untagged)
    assert reviewer == heads.OTHER[worker]
    assert {vendor for vendor, tag in launches() if tag is not None} == {'claude'}


@pytest.mark.parametrize('setup,review,expected,reason', [
    ({'claude': {'tag_launcher': False}}, False, [], 'worker vendors claude, codex'),
    ({'claude': {'disabled': True}}, False, [], 'worker vendors claude, codex'),
    ({}, True, [('worker', 'claude')], 'reviewer vendors codex'),
], ids=['no_launcher', 'launcher_disabled', 'review_on_the_other_vendor'])
def test_tagged_card_waits_naming_the_tag_when_no_tag_capable_vendor_is_allowed(env, tmp_path, setup, review, expected,
                                                                                reason):
    cfg, conn, d = env
    for vendor, keys in setup.items():
        cfg['vendors'][vendor].update(keys)
    launches = install_launchers(cfg, tmp_path)
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] tagged', review=review, tag=TAG)
    assert tick_until_terminal(d, conn, cid) == 'blocked'
    card = api.card_row(conn, cid)
    assert (card['wait_reason'], card['block_reason']) == ('tag', f'no tag launcher for tag {TAG} among {reason}')
    assert waiting(conn, cid, time.time()) == {'badge': 'external', 'cause': 'tag', 'reason': card['block_reason'],
                                               'until': None}
    assert heads_of(conn, cid) == expected
    assert launches() == [(vendor, TAG) for _, vendor in expected]
    untagged = mk(conn, '[ok] untagged')
    assert tick_until_terminal(d, conn, untagged) == 'done'
