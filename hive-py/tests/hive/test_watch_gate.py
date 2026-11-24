"""Watch gate decisions, handoff acknowledgement and checkpoint persistence through the public API."""
import json
import threading
import time
from datetime import datetime, time as clock
from zoneinfo import ZoneInfo

import pytest
from hive import watch_gate as wg

ZONE = ZoneInfo('UTC')
INBOX = wg.Policy(news_destination='owner-inbox', lease=600)
SHORT_NIGHT = wg.QuietWindow(start=clock(1), end=clock(8), tz=ZONE.key)
LONG_NIGHT = wg.QuietWindow(start=clock(0), end=clock(9), tz=ZONE.key)
NIGHT = wg.Policy(news_destination='telegram', lease=600, quiet=SHORT_NIGHT)
T0 = 1_900_000_000.0


def _corrupt(edit):
    cp = wg.baseline([news('a')], T0)
    cp = wg.begin(cp, wg.evaluate(cp, [news('a', 'edited')], T0, INBOX), 'g1', T0, INBOX)
    data = json.loads(cp.to_json())
    edit(data)
    return json.dumps(data)


def news(key, text='x', wake_at=None, bypass=False):
    return wg.Signal(key, 'news', text, wake_at, bypass=bypass)


def fault(key, text='down', covers=()):
    return wg.Signal(key, 'source_error', text, covers=covers)


CORRUPT_RECORD = _corrupt(lambda d: d['baseline']['a'].update(fired='1900000000'))
CORRUPT_LEASE = _corrupt(lambda d: d['pending'].update(lease_until=None))
CORRUPT_FLAG = _corrupt(lambda d: d['pending']['items'][0].update(held='false'))


def local(hour, minute=0):
    return datetime(2030, 1, 15, hour, minute, tzinfo=ZONE).timestamp()


def deliver(cp, signals, now, policy=INBOX, gate='g'):
    """One tick whose head succeeds; returns the decision and the resulting checkpoint."""
    decision = wg.evaluate(cp, signals, now, policy)
    if decision.action == 'wake':
        return decision, wg.ack(wg.begin(cp, decision, gate, now, policy), gate)
    return decision, wg.observe(cp, decision)


def changes(decision):
    return [(i.key, i.change, i.destination, i.held, i.redelivered) for i in decision.items]


def test_unchanged_input_across_many_ticks_never_wakes():
    signals = [news('hive:h_1:blocked', 'blocked: disk'), news('promise:abc', 'send report')]
    cp = wg.baseline(signals, T0)
    for n in range(200):
        decision, cp = deliver(cp, signals, T0 + n * 1800)
        assert decision.action == 'silent' and decision.items == ()


def test_timestamp_drift_normalizes_to_identical_silent_decision():
    def collect(raw):
        return [news(f"card:{c['ref']}:{c['state']}", c['title']) for c in raw['cards']]

    first = {'taken_at': '2030-01-01T00:00:00Z', 'pid': 41,
             'cards': [{'ref': 'a-1', 'state': 'blocked', 'title': 'Backup', 'age_s': 3700}]}
    second = {'taken_at': '2030-01-01T00:30:00Z', 'pid': 97,
              'cards': [{'ref': 'a-1', 'state': 'blocked', 'title': 'Backup', 'age_s': 5500}]}
    cp = wg.baseline(collect(first), T0)
    one = wg.evaluate(cp, collect(second), T0 + 1800, INBOX)
    two = wg.evaluate(cp, list(reversed(collect(second))), T0 + 1800, INBOX)
    assert one.action == 'silent'
    assert one.to_json() == two.to_json()


def test_new_changed_and_cleared_keys_wake_with_destinations():
    cp = wg.baseline([news('a', 'one'), news('b', 'two')], T0)
    decision = wg.evaluate(cp, [news('a', 'one!'), news('c', 'three')], T0, NIGHT)
    assert decision.action == 'wake'
    assert changes(decision) == [('a', 'changed', 'telegram', False, False),
                                 ('b', 'cleared', 'telegram', False, False),
                                 ('c', 'new', 'telegram', False, False)]
    assert decision.items[0].before.fingerprint == 'one' and decision.items[1].after is None
    cp = wg.ack(wg.begin(cp, decision, 'g1', T0, NIGHT), 'g1')
    assert wg.evaluate(cp, [news('a', 'one!'), news('c', 'three')], T0 + 1, NIGHT).action == 'silent'


def test_failed_handoff_retries_pending_diff_merged_with_new_diff():
    cp = wg.baseline([news('a')], T0)
    first = wg.evaluate(cp, [news('a'), news('b')], T0, INBOX)
    cp = wg.begin(cp, first, 'g1', T0, INBOX)
    assert set(cp.baseline) == {'a'}
    cp = wg.fail(cp, 'g1')
    retry = wg.evaluate(cp, [news('a'), news('b'), news('c')], T0 + 60, INBOX)
    assert retry.action == 'wake'
    assert changes(retry) == [('b', 'new', 'owner-inbox', False, False), ('c', 'new', 'owner-inbox', False, False)]
    cp = wg.ack(wg.begin(cp, retry, 'g2', T0 + 60, INBOX), 'g2')
    assert wg.evaluate(cp, [news('a'), news('b'), news('c')], T0 + 120, INBOX).action == 'silent'


def test_failed_item_that_vanished_is_redelivered_once():
    cp = wg.baseline([], T0)
    cp = wg.fail(wg.begin(cp, wg.evaluate(cp, [news('flash')], T0, INBOX), 'g1', T0, INBOX), 'g1')
    retry = wg.evaluate(cp, [], T0 + 60, INBOX)
    assert retry.action == 'wake' and changes(retry) == [('flash', 'new', 'owner-inbox', False, True)]
    cp = wg.ack(wg.begin(cp, retry, 'g2', T0 + 60, INBOX), 'g2')
    assert wg.evaluate(cp, [], T0 + 120, INBOX).action == 'silent'


def test_in_flight_handoff_blocks_second_wake_and_keeps_newer_change():
    cp = wg.baseline([], T0)
    cp = wg.begin(cp, wg.evaluate(cp, [news('a')], T0, INBOX), 'g1', T0, INBOX)
    busy = wg.evaluate(cp, [news('a'), news('b')], T0 + 60, INBOX)
    assert (busy.action, busy.gate) == ('wait-active', 'g1')
    with pytest.raises(wg.GateError):
        wg.begin(cp, busy, 'g2', T0 + 60, INBOX)
    cp = wg.ack(cp, 'g1')
    later = wg.evaluate(cp, [news('a'), news('b')], T0 + 120, INBOX)
    assert later.action == 'wake' and changes(later) == [('b', 'new', 'owner-inbox', False, False)]


def test_change_seen_in_flight_is_owed_after_ack_even_when_its_signal_vanished():
    cp = wg.baseline([], T0)
    cp = wg.begin(cp, wg.evaluate(cp, [news('a')], T0, INBOX), 'g1', T0, INBOX)
    busy, cp = deliver(cp, [news('a'), news('b')], T0 + 10, INBOX)
    assert busy.action == 'wait-active'
    cp = wg.ack(cp, 'g1')
    later, cp = deliver(cp, [news('a')], T0 + 20, INBOX)
    assert later.action == 'wake' and changes(later) == [('b', 'new', 'owner-inbox', False, True)]
    assert wg.evaluate(cp, [news('a')], T0 + 30, INBOX).action == 'silent'


def test_value_changed_and_reverted_in_flight_is_owed_beside_the_current_diff():
    cp = wg.baseline([news('a', 'v0')], T0)
    cp = wg.begin(cp, wg.evaluate(cp, [news('a', 'v1')], T0, INBOX), 'g1', T0, INBOX)
    _, cp = deliver(cp, [news('a', 'v2')], T0 + 10, INBOX)
    cp = wg.ack(cp, 'g1')
    later, cp = deliver(cp, [news('a', 'v1')], T0 + 20, INBOX)
    assert changes(later) == [('a', 'changed', 'owner-inbox', False, True)]
    assert later.items[0].after.fingerprint == 'v2'
    assert wg.evaluate(cp, [news('a', 'v1')], T0 + 30, INBOX).action == 'silent'


def test_expired_lease_wakes_again_with_abandoned_items():
    cp = wg.baseline([], T0)
    cp = wg.begin(cp, wg.evaluate(cp, [news('a')], T0, INBOX), 'g1', T0, INBOX)
    assert wg.evaluate(cp, [news('a')], T0 + 599, INBOX).action == 'wait-active'
    retry = wg.evaluate(cp, [news('a')], T0 + 600, INBOX)
    assert retry.action == 'wake' and changes(retry) == [('a', 'new', 'owner-inbox', False, False)]
    cp = wg.begin(cp, retry, 'g2', T0 + 600, INBOX)
    with pytest.raises(wg.GateError):
        wg.ack(cp, 'g1')


def test_wake_at_crossing_on_unchanged_input_wakes_exactly_once():
    signal = news('card:a-1:blocked', 'blocked: disk', wake_at=T0 + 3600)
    cp = wg.baseline([], T0)
    wakes = []
    for n in range(12):
        decision, cp = deliver(cp, [signal], T0 + n * 900)
        if decision.action == 'wake':
            wakes.append((n, changes(decision)))
    assert wakes == [(4, [('card:a-1:blocked', 'threshold', 'owner-inbox', False, False)])]


@pytest.mark.parametrize('window', [SHORT_NIGHT, LONG_NIGHT])
def test_night_news_is_held_and_wakes_at_window_end_on_unchanged_input(window):
    policy = wg.Policy(news_destination='telegram', lease=600, quiet=window)
    end = window.end.hour
    cp = wg.baseline([], T0)
    signals = [news('src:issue:comment:1', 'reply from maintainer')]
    decisions = []
    for hour, minute in ((2, 0), (5, 30), (end - 1, 59), (end, 0), (end + 1, 0)):
        decision, cp = deliver(cp, signals, local(hour, minute), policy)
        decisions.append((decision.action, [i.held for i in decision.items]))
    assert decisions == [('silent', [True]), ('silent', [True]), ('silent', [True]), ('wake', [False]),
                         ('silent', [])]


@pytest.mark.parametrize('window', [SHORT_NIGHT, LONG_NIGHT])
def test_night_news_that_vanishes_overnight_still_wakes_at_window_end(window):
    policy = wg.Policy(news_destination='telegram', lease=600, quiet=window)
    cp = wg.baseline([], T0)
    log = []
    for hour, signals in ((2, [news('issue:title', 'renamed')]), (3, []), (window.end.hour, []),
                          (window.end.hour + 1, [])):
        decision, cp = deliver(cp, signals, local(hour), policy)
        log.append((decision.action, changes(decision)))
    assert log == [('silent', [('issue:title', 'new', 'telegram', True, False)]),
                   ('silent', [('issue:title', 'new', 'telegram', True, True)]),
                   ('wake', [('issue:title', 'new', 'telegram', False, True)]),
                   ('silent', [])]


def test_bypass_signal_wakes_inside_the_window_while_plain_news_waits():
    policy = wg.Policy(news_destination='telegram', lease=600, quiet=LONG_NIGHT)
    cp = wg.baseline([], T0)
    signals = [news('issue:comment:1', 'may we merge your change?', bypass=True), news('issue:label', 'triaged')]
    decision = wg.evaluate(cp, signals, local(0, 30), policy)
    assert decision.action == 'wake' and [i.key for i in decision.handed] == ['issue:comment:1']
    cp = wg.fail(wg.begin(cp, decision, 'g1', local(0, 30), policy), 'g1')
    retry, cp = deliver(cp, [], local(1), policy)
    assert changes(retry) == [('issue:comment:1', 'new', 'telegram', False, True),
                              ('issue:label', 'new', 'telegram', True, True)]
    morning, cp = deliver(cp, [], local(9), policy)
    assert changes(morning) == [('issue:label', 'new', 'telegram', False, True)]


def test_night_source_error_wakes_while_news_stays_held():
    cp = wg.baseline([], T0)
    signals = [news('comment:1', 'reply'), fault('src:github', 'api 502')]
    decision = wg.evaluate(cp, signals, local(3), NIGHT)
    assert decision.action == 'wake'
    assert [i.key for i in decision.handed] == ['src:github']
    cp = wg.ack(wg.begin(cp, decision, 'g1', local(3), NIGHT), 'g1')
    assert wg.evaluate(cp, signals, local(4), NIGHT).action == 'silent'
    morning = wg.evaluate(cp, signals, local(8), NIGHT)
    assert morning.action == 'wake' and changes(morning) == [('comment:1', 'new', 'telegram', False, False)]


def test_source_failure_persistence_and_recovery_route_to_owner_inbox():
    policy = wg.Policy(news_destination='telegram', lease=600)
    cp = wg.baseline([news('comment:1')], T0)
    up = [news('comment:1')]
    down = [news('comment:1'), fault('src:github', 'api 502')]
    log = []
    for n, signals in enumerate([down, down, down, up, up]):
        decision, cp = deliver(cp, signals, T0 + n * 60, policy)
        log.append((decision.action, changes(decision)))
    assert log == [('wake', [('src:github', 'new', 'owner-inbox', False, False)]),
                   ('silent', []), ('silent', []),
                   ('wake', [('src:github', 'cleared', 'owner-inbox', False, False)]),
                   ('silent', [])]


def test_collector_failure_signal_keeps_covered_keys_from_clearing():
    cp = wg.baseline([news('hive:h_1:blocked'), news('promise:p1')], T0)
    failed = [fault('src:hive', 'observe timed out', covers=('hive:',))]
    decision, cp = deliver(cp, failed, T0)
    assert changes(decision) == [('promise:p1', 'cleared', 'owner-inbox', False, False),
                                 ('src:hive', 'new', 'owner-inbox', False, False)]
    decision, cp = deliver(cp, [], T0 + 60)
    assert changes(decision) == [('hive:h_1:blocked', 'cleared', 'owner-inbox', False, False),
                                 ('src:hive', 'cleared', 'owner-inbox', False, False)]


def test_missing_checkpoint_needs_explicit_baseline_recorded_silently(tmp_path):
    path = tmp_path / 'gate.json'
    with pytest.raises(wg.CheckpointMissing):
        wg.read(path)
    with pytest.raises(wg.CheckpointMissing):
        with wg.locked(path):
            pass
    assert not path.exists()
    signals = [news('comment:1', 'old'), news('comment:2', 'old'), news('comment:3', 'fresh')]
    with wg.locked(path, baseline=wg.baseline(signals, T0, seen=['comment:1', 'comment:2', 'gone'])):
        pass
    decision = wg.evaluate(wg.read(path), signals, T0, INBOX)
    assert changes(decision) == [('comment:3', 'new', 'owner-inbox', False, False)]


@pytest.mark.parametrize('text', ['', '{"version": 1', '[]', '{"version": 2, "baseline": {}, "owed": [], "pending": null}',
                                  '{"version": 1, "baseline": {"a": {"class": "bogus"}}, "owed": [], "pending": null}',
                                  '{"version": true, "baseline": {}, "owed": [], "pending": null}',
                                  CORRUPT_RECORD, CORRUPT_LEASE, CORRUPT_FLAG])
def test_corrupt_checkpoint_is_refused_never_empty(tmp_path, text):
    path = tmp_path / 'gate.json'
    path.write_text(text)
    with pytest.raises(wg.CheckpointCorrupt):
        wg.read(path)
    with pytest.raises(wg.CheckpointCorrupt):
        with wg.locked(path, baseline=wg.baseline([], T0)):
            pass
    assert path.read_text() == text


@pytest.mark.parametrize('lease', [0, -5, float('inf'), float('nan'), True, '600'])
def test_policy_refuses_a_lease_that_is_not_a_finite_positive_number(lease):
    with pytest.raises(ValueError):
        wg.Policy(news_destination='owner-inbox', lease=lease)


def test_pending_handoff_survives_reload_and_read_only_evaluation_writes_nothing(tmp_path):
    path = tmp_path / 'gate.json'
    with wg.locked(path, baseline=wg.baseline([news('a')], T0)) as tx:
        decision = wg.evaluate(tx.checkpoint, [news('a', 'edited')], T0, INBOX)
        tx.commit(wg.begin(tx.checkpoint, decision, 'g1', T0, INBOX))
    before = sorted((p.name, p.read_bytes()) for p in tmp_path.iterdir())
    probe = wg.evaluate(wg.read(path), [news('a', 'edited')], T0 + 60, INBOX)
    assert (probe.action, probe.gate) == ('wait-active', 'g1')
    assert sorted((p.name, p.read_bytes()) for p in tmp_path.iterdir()) == before
    with wg.locked(path) as tx:
        tx.commit(wg.fail(tx.checkpoint, 'g1'))
    retry = wg.evaluate(wg.read(path), [news('a', 'edited')], T0 + 120, INBOX)
    assert retry.action == 'wake' and changes(retry) == [('a', 'changed', 'owner-inbox', False, False)]


def test_concurrent_committers_are_serialized_by_the_lock(tmp_path):
    path = tmp_path / 'gate.json'
    with wg.locked(path, baseline=wg.baseline([], T0)):
        pass
    errors = []

    def add(n):
        try:
            with wg.locked(path) as tx:
                cp = tx.checkpoint
                signals = [news(k) for k in cp.baseline] + [news(f'k{n}')]
                time.sleep(0.01)
                decision = wg.evaluate(cp, signals, T0, INBOX)
                tx.commit(wg.ack(wg.begin(cp, decision, f'g{n}', T0, INBOX), f'g{n}'))
        except Exception as e:
            errors.append(e)

    threads = [threading.Thread(target=add, args=(n,)) for n in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert errors == []
    assert sorted(wg.read(path).baseline) == [f'k{n}' for n in range(8)]
    assert sorted(p.name for p in tmp_path.iterdir()) == ['gate.json', 'gate.json.lock']


def test_ack_and_fail_refuse_an_unknown_gate():
    cp = wg.baseline([], T0)
    with pytest.raises(wg.GateError):
        wg.ack(cp, 'g1')
    cp = wg.begin(cp, wg.evaluate(cp, [news('a')], T0, INBOX), 'g1', T0, INBOX)
    with pytest.raises(wg.GateError):
        wg.fail(cp, 'other')
    with pytest.raises(wg.GateError):
        wg.begin(wg.baseline([], T0), wg.evaluate(wg.baseline([], T0), [], T0, INBOX), 'g2', T0, INBOX)


def test_handoff_payload_is_json_with_items_and_signals():
    cp = wg.baseline([], T0)
    decision = wg.evaluate(cp, [news('b'), fault('src:x')], T0, INBOX)
    payload = json.loads(wg.begin(cp, decision, 'g1', T0, INBOX).to_json())['pending']
    assert payload['gate'] == 'g1'
    assert [i['key'] for i in payload['items']] == ['b', 'src:x']
    assert [s['key'] for s in payload['signals']] == ['b', 'src:x']


QUIET_NEWS = wg.Policy(news_destination='owner-inbox', lease=600, wake_on_cleared=False, error_grace=7200)


def test_cleared_only_change_never_wakes_and_leaves_the_baseline_when_opted_out():
    cp = wg.baseline([news('a'), news('b')], T0)
    decision = wg.evaluate(cp, [news('a')], T0 + 60, QUIET_NEWS)
    assert decision.action == 'silent' and changes(decision) == [('b', 'cleared', 'owner-inbox', False, False)]
    cp = wg.observe(cp, decision)
    assert 'b' not in cp.baseline and cp.owed == ()
    # the key coming back is news again, not an unchanged baseline entry
    assert changes(wg.evaluate(cp, [news('a'), news('b')], T0 + 120, QUIET_NEWS)) == [
        ('b', 'new', 'owner-inbox', False, False)]
    # default policy keeps waking on a cleared key
    assert wg.evaluate(wg.baseline([news('a'), news('b')], T0), [news('a')], T0 + 60, INBOX).action == 'wake'


def test_cleared_key_rides_along_with_real_news_and_leaves_the_baseline_on_ack():
    cp = wg.baseline([news('a'), news('b')], T0)
    decision, cp = deliver(cp, [news('a'), news('c')], T0 + 60, QUIET_NEWS)
    assert decision.action == 'wake'
    assert changes(decision) == [('b', 'cleared', 'owner-inbox', False, False),
                                 ('c', 'new', 'owner-inbox', False, False)]
    assert set(cp.baseline) == {'a', 'c'}


def test_cleared_source_error_alone_is_absorbed_and_a_cleared_item_in_flight_is_still_owed():
    cp = wg.baseline([news('a'), fault('src:x', 'down')], T0)
    decision = wg.evaluate(cp, [news('a')], T0 + 60, QUIET_NEWS)
    assert decision.action == 'silent' and set(wg.observe(cp, decision).baseline) == {'a'}
    cp = wg.begin(wg.baseline([news('a')], T0), wg.evaluate(wg.baseline([news('a')], T0), [news('a'), news('n')],
                                                            T0 + 1, QUIET_NEWS), 'g1', T0 + 1, QUIET_NEWS)
    decision = wg.evaluate(cp, [news('a'), news('n')], T0 + 2, QUIET_NEWS)
    assert decision.action == 'wait-active' and decision.absorbed == ()


def test_flapping_source_error_stays_silent_and_a_persistent_one_wakes_once():
    cp = wg.baseline([news('a')], T0)
    log = []
    # present one tick, absent the next, over and over: never reaches the grace period
    for n in range(8):
        signals = [news('a'), fault('src:deleg', 'invalid output')] if n % 2 == 0 else [news('a')]
        decision, cp = deliver(cp, signals, T0 + n * 1800, QUIET_NEWS)
        log.append(decision.action)
    assert set(log) == {'silent'} and cp.suspects == {}
    # present for good: silent while younger than the grace, one wake once it is older, then acknowledged
    log, down = [], [news('a'), fault('src:deleg', 'invalid output')]
    for n in range(8):
        decision, cp = deliver(cp, down, T0 + 20000 + n * 1800, QUIET_NEWS)
        log.append((decision.action, changes(decision)))
    assert [a for a, _ in log] == ['silent'] * 4 + ['wake'] + ['silent'] * 3
    assert log[4][1] == [('src:deleg', 'new', 'owner-inbox', False, False)]
    assert cp.suspects == {}


def test_error_grace_holds_a_changed_error_and_tracks_first_sight_through_a_reload(tmp_path):
    path = tmp_path / 'cp.json'
    cp = wg.baseline([fault('src:x', 'a')], T0)
    cp = wg.observe(cp, wg.evaluate(cp, [fault('src:x', 'b')], T0 + 10, QUIET_NEWS))
    assert cp.suspects == {'src:x': T0 + 10}
    path.write_text(cp.to_json())
    cp = wg.read(path)
    assert cp.suspects == {'src:x': T0 + 10}
    # another text of the same still-failing source keeps the first-seen time
    assert wg.evaluate(cp, [fault('src:x', 'c')], T0 + 7000, QUIET_NEWS).action == 'silent'
    assert wg.evaluate(cp, [fault('src:x', 'c')], T0 + 7300, QUIET_NEWS).action == 'wake'


def test_policy_refuses_bad_grace_and_wake_flag():
    for bad in (-1, float('nan'), True, '60'):
        with pytest.raises(ValueError):
            wg.Policy('owner-inbox', 600, error_grace=bad)
    with pytest.raises(ValueError):
        wg.Policy('owner-inbox', 600, wake_on_cleared='no')


def test_checkpoint_without_suspects_stays_byte_identical_and_old_files_load():
    cp = wg.baseline([news('a')], T0)
    assert 'suspects' not in json.loads(cp.to_json())
    assert wg.Checkpoint.from_dict(json.loads(cp.to_json())).suspects == {}


def test_stale_owed_cleared_for_a_returned_key_is_dropped_not_absorbed_into_the_baseline():
    cp = wg.baseline([news('a'), news('k')], T0)
    decision = wg.evaluate(cp, [news('a'), news('n')], T0 + 1, QUIET_NEWS)
    # the handoff fails, so k's clearing is owed; then k returns unchanged
    failed = wg.fail(wg.begin(cp, decision, 'g1', T0 + 1, QUIET_NEWS), 'g1')
    assert any(i.key == 'k' and i.change == 'cleared' for i in failed.owed)
    back = [news('a'), news('k'), news('n')]
    decision = wg.evaluate(failed, back, T0 + 3, QUIET_NEWS)
    assert [(i.key, i.change) for i in decision.items] == [('n', 'new')]
    acked = wg.ack(wg.begin(failed, decision, 'g2', T0 + 3, QUIET_NEWS), 'g2')
    assert 'k' in acked.baseline and acked.owed == ()
    assert wg.evaluate(acked, back, T0 + 4, QUIET_NEWS).action == 'silent'
    # on a silent tick the stale item leaves the owed list and the live key stays in the baseline
    only_cleared = wg.Checkpoint({k: v for k, v in failed.baseline.items()},
                                 tuple(i for i in failed.owed if i.change == 'cleared'))
    quiet = wg.evaluate(only_cleared, [news('a'), news('k')], T0 + 6, QUIET_NEWS)
    assert quiet.action == 'silent' and quiet.items == ()
    after = wg.observe(only_cleared, quiet)
    assert set(after.baseline) == {'a', 'k'} and after.owed == ()
    assert wg.evaluate(after, [news('a'), news('k')], T0 + 7, QUIET_NEWS).action == 'silent'


def test_acknowledged_error_restarts_its_grace_so_a_changing_error_text_wakes_once_per_grace():
    cp = wg.baseline([], T0)
    log = []
    for n in range(10):
        decision, cp = deliver(cp, [fault('src:x', f'text {n}')], T0 + n * 3600, QUIET_NEWS)
        log.append(decision.action)
    assert log == ['silent', 'silent', 'wake', 'silent', 'silent', 'wake', 'silent', 'silent', 'wake', 'silent']
