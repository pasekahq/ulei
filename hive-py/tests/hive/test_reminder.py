"""Reminders (ADR-0100): at not_before a reminder publishes one `reminder_due` carrying its text verbatim and starts no
head; the secretary claims it, then sends or withdraws it; unclaimed or silent, the hive sends the original text through
`ulei-tg notify` and tells the secretary. Delivered only with a Telegram receipt."""
import json
from pathlib import Path

import pytest
from fake_delivery import FakeAdapter
from test_delivery_adapter import c1, ulei_keys  # noqa: F401
from test_hive import env, fake_time, mk, mk_series, record_spawns  # noqa: F401
from hive import api, delivery, dispatcher, store

TEXT = '  Synthetic reminder: water the plants «ёлки» ✓\n'
START = 1000.0
EXAMPLE = Path(__file__).resolve().parents[2] / 'config.example.toml'


@pytest.fixture
def hive(env, monkeypatch, tmp_path):
    """(cfg, conn, d, now, adapter): the test hive with `now` its clock and time.time, at START, and its events handed to
    `adapter`; the CLI reads the same store and the same fake ulei-tg."""
    cfg, conn, _ = env
    adapter = FakeAdapter()
    monkeypatch.setitem(delivery.ADAPTERS, 'fake', lambda c: adapter)
    cfg = dict(cfg, delivery='fake')
    d = dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {})
    now = fake_time(monkeypatch, d, START)
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    return cfg, conn, d, now, adapter


def remind(conn, at=START, **kw):
    cid, _ = api.create(conn, title='synthetic reminder', goal='remind the owner', step=1, source='system:test',
                        created_by='secretary', card_class='owner_task', not_before=at, reminder=kw.pop('text', TEXT),
                        **kw)
    return cid


def tg_calls(tmp_path):
    calls = tmp_path / 'tg' / 'calls.jsonl'
    return [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []


def tg_answers(tmp_path, *answers):
    (tmp_path / 'tg' / 'exits.json').write_text(json.dumps(list(answers)))


def at(d, now, t, ticks=1):
    now[0] = t
    for _ in range(ticks):
        d.tick()


def cli(capsys, *argv):
    from scripts import hive as cli_module
    code = cli_module.main(list(argv))
    out, err = capsys.readouterr()
    return code, out, err


def events(adapter, kind):
    return [(e.id, json.loads(e.payload)) for e in adapter.events if e.kind == kind]


def receipt(message_id):
    return {'chats': [{'chat_id': 4242, 'message_ids': [message_id]}]}


def test_reminder_publishes_one_reminder_due_with_its_text_and_starts_no_head(hive, tmp_path):
    _, conn, d, now, adapter = hive
    spawned = record_spawns(d)
    cid = remind(conn, at=START + 60)
    plain = mk(conn, 'plain card with a time', not_before=START + 60)
    at(d, now, START)
    assert api.get(conn, cid)['state'] == 'created' and adapter.events == [] and spawned == []
    at(d, now, START + 60, ticks=3)
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason'], card['reminder']) == ('blocked', 'reminder', TEXT)
    assert card['reminder_state']['state'] == 'due' and card['runs'] == []
    [(event_id, payload)] = events(adapter, 'reminder_due')
    assert event_id == f'hive:{cid}:reminder_due'
    assert (payload['text'], payload['card_id'], payload['due_at']) == (TEXT, cid, START + 60)
    assert (payload['series'], payload['period'], payload['unclaimed_s'], payload['claim_silence_s']) == (None, None, 180, 300)
    assert {e.kind for e in adapter.events} == {'reminder_due'} and tg_calls(tmp_path) == []
    other = api.get(conn, plain)
    assert other['state'] == 'running' and len(spawned) == 1 and other['reminder_state'] is None


def test_unclaimed_reminder_is_sent_once_directly_and_the_secretary_told(hive, tmp_path):
    cfg, conn, d, now, adapter = hive
    cid = remind(conn)
    at(d, now, START)
    at(d, now, START + 179)
    assert tg_calls(tmp_path) == []
    at(d, now, START + 180, ticks=2)
    assert tg_calls(tmp_path) == [{'argv': ['notify', '--config', cfg['reminders']['tg_config']], 'stdin': TEXT}]
    card = api.get(conn, cid)
    assert (card['state'], card['result']) == ('done', TEXT)
    sent = card['reminder_state']
    assert (sent['state'], sent['sent_by'], sent['sent_text'], sent['receipt']) == ('delivered', 'dispatcher', TEXT, receipt(101))
    [(event_id, payload)] = events(adapter, 'reminder_fallback')
    assert event_id == f'hive:{cid}:reminder_fallback'
    assert (payload['text'], payload['why'], payload['due_at'], payload['claimed_at'], payload['sent_at'], payload['receipt']) == (
        TEXT, 'unclaimed', START, None, START + 180, receipt(101))
    at(d, now, START + 7200, ticks=2)
    assert len(tg_calls(tmp_path)) == 1 and len(events(adapter, 'reminder_fallback')) == 1
    assert {e.kind for e in adapter.events} == {'reminder_due', 'reminder_fallback'}


def test_claim_holds_the_fallback_until_the_claim_falls_silent(hive, tmp_path, capsys):
    _, conn, d, now, adapter = hive
    cid = remind(conn)
    at(d, now, START)
    now[0] = START + 100
    assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    at(d, now, START + 180)
    at(d, now, START + 399)
    assert tg_calls(tmp_path) == [] and api.get(conn, cid)['reminder_state']['state'] == 'claimed'
    at(d, now, START + 400)
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [TEXT]
    [(_, payload)] = events(adapter, 'reminder_fallback')
    assert (payload['why'], payload['claimed_at'], payload['text']) == ('claim_silent', START + 100, TEXT)
    assert api.get(conn, cid)['reminder_state']['state'] == 'delivered'


def test_withdrawn_or_cancelled_reminder_is_never_sent(hive, tmp_path, capsys):
    _, conn, d, now, adapter = hive
    withdrawn, cancelled = remind(conn), remind(conn)
    at(d, now, START)
    assert cli(capsys, 'reminder', 'claim', withdrawn)[0] == 0
    code, out, _ = cli(capsys, 'reminder', 'withdraw', withdrawn, '--reason', 'the owner already did it')
    assert code == 0 and out == f'{withdrawn} reminder withdrawn\n'
    assert cli(capsys, 'cancel', cancelled, '--reason', 'not needed')[0] == 0
    at(d, now, START + 86400, ticks=2)
    assert tg_calls(tmp_path) == [] and events(adapter, 'reminder_fallback') == []
    card = api.get(conn, withdrawn)
    assert (card['state'], card['result']) == ('cancelled', 'the owner already did it')
    assert (card['reminder_state']['state'], card['reminder_state']['reason']) == ('withdrawn', 'the owner already did it')
    assert card['reminder_state']['receipt'] is None
    assert api.get(conn, cancelled)['reminder_state']['state'] == 'due'


@pytest.mark.parametrize('answer,error', [
    (1, 'ulei-tg notify: exit 1: ulei-tg notify: sent 0 of 1 messages, then telegram network: synthetic failure 1'),
    ('no-receipt', 'ulei-tg notify: exit 0 without a receipt'),
])
def test_failed_send_is_not_delivered_and_is_retried_on_a_later_tick(hive, tmp_path, answer, error):
    _, conn, d, now, adapter = hive
    tg_answers(tmp_path, answer)
    cid = remind(conn)
    at(d, now, START)
    at(d, now, START + 180, ticks=2)
    assert len(tg_calls(tmp_path)) == 1
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason']) == ('blocked', 'reminder')
    failed = card['reminder_state']
    assert (failed['state'], failed['attempts'], failed['error'], failed['retry_at']) == ('due', 1, error, START + 240)
    assert (failed['receipt'], failed['sent_by'], failed['sent_at']) == (None, None, None)
    assert events(adapter, 'reminder_fallback') == []
    at(d, now, START + 239)
    assert len(tg_calls(tmp_path)) == 1
    at(d, now, START + 240)
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [TEXT, TEXT]
    sent = api.get(conn, cid)['reminder_state']
    assert (sent['state'], sent['attempts'], sent['error'], sent['receipt']) == ('delivered', 1, None, receipt(102))
    assert len(events(adapter, 'reminder_fallback')) == 1


def test_reminder_failing_max_attempts_blocks_the_card_and_tells_the_secretary(hive, tmp_path):
    cfg, conn, d, now, adapter = hive
    cfg['reminders']['max_attempts'] = 2
    tg_answers(tmp_path, 1, 3)
    cid = remind(conn)
    at(d, now, START)
    at(d, now, START + 180)
    at(d, now, START + 240)
    card = api.get(conn, cid)
    assert (card['state'], card['wait_reason'], card['reminder_state']['state']) == ('blocked', None, 'failed')
    assert card['block_reason'].startswith('reminder not delivered after 2 attempts: ulei-tg notify: exit 3')
    assert [e.id for e in adapter.events if e.kind == 'task_blocked'] == [f'hive:{cid}:blocked']
    at(d, now, START + 7200)
    assert len(tg_calls(tmp_path)) == 2


def test_delivered_reminder_is_not_sent_again_after_a_dispatcher_restart(hive, tmp_path):
    cfg, conn, d, now, adapter = hive
    cid = remind(conn)
    at(d, now, START)
    at(d, now, START + 180)
    assert len(tg_calls(tmp_path)) == 1
    restarted = dispatcher.Dispatcher(cfg, store.connect(cfg['db']), limits_fetcher=lambda c: {})
    restarted.clock = lambda: now[0]
    at(restarted, now, START + 86400, ticks=3)
    assert len(tg_calls(tmp_path)) == 1 and len(events(adapter, 'reminder_fallback')) == 1
    assert api.get(conn, cid)['reminder_state']['state'] == 'delivered'


def test_send_delivers_the_original_or_edited_text_and_records_the_receipt(hive, tmp_path, capsys):
    _, conn, d, now, adapter = hive
    original, edited = remind(conn), remind(conn)
    at(d, now, START)
    for cid in (original, edited):
        assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    code, out, err = cli(capsys, 'reminder', 'send', original)
    assert (code, err) == (0, '') and json.loads(out) == {'card': original, 'state': 'delivered', 'receipt': receipt(101)}
    code, out, _ = cli(capsys, 'reminder', 'send', edited, '--text', 'Water the plants now.')
    assert code == 0 and json.loads(out)['receipt'] == receipt(102)
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [TEXT, 'Water the plants now.']
    for cid, text, message in ((original, TEXT, 101), (edited, 'Water the plants now.', 102)):
        card = api.get(conn, cid)
        sent = card['reminder_state']
        assert (card['state'], card['result']) == ('done', text)
        assert (sent['state'], sent['sent_by'], sent['sent_text'], sent['receipt']) == ('delivered', 'secretary', text,
                                                                                        receipt(message))
    at(d, now, START + 86400)
    assert len(tg_calls(tmp_path)) == 2 and events(adapter, 'reminder_fallback') == []


def test_failed_secretary_send_exits_1_and_the_fallback_sends_later(hive, tmp_path, capsys):
    _, conn, d, now, adapter = hive
    tg_answers(tmp_path, 1)
    cid = remind(conn)
    at(d, now, START)
    assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    code, out, err = cli(capsys, 'reminder', 'send', cid)
    assert (code, out) == (1, '')
    assert err == ('hive: reminder send failed: ulei-tg notify: exit 1: ulei-tg notify: sent 0 of 1 messages, '
                   'then telegram network: synthetic failure 1\n')
    state = api.get(conn, cid)['reminder_state']
    assert (state['state'], state['attempts'], state['receipt']) == ('claimed', 1, None)
    at(d, now, START + 300)
    assert len(tg_calls(tmp_path)) == 2 and api.get(conn, cid)['reminder_state']['sent_by'] == 'dispatcher'


def test_verbs_refuse_a_card_that_is_not_a_reminder_or_not_in_their_state(hive, tmp_path, capsys):
    _, conn, d, now, _ = hive
    plain = mk(conn, 'plain card', not_before=START + 3600)
    later, due, claimed = remind(conn, at=START + 3600), remind(conn), remind(conn)
    at(d, now, START)
    assert cli(capsys, 'reminder', 'claim', claimed)[0] == 0
    before = {cid: (api.get(conn, cid)['state'], api.get(conn, cid)['reminder_state']) for cid in (plain, later, due, claimed)}
    for argv, error in [
        (('claim', plain), f'card {plain} is not a reminder'),
        (('claim', later), f'reminder {later} is not due yet (card created)'),
        (('send', due), f'reminder {due} is due; this needs claimed'),
        (('withdraw', due, '--reason', 'no longer needed'), f'reminder {due} is due; this needs claimed'),
        (('claim', claimed), f'reminder {claimed} is claimed; this needs due'),
        (('claim', due, '--by', 'stranger'), "actor must be secretary or owner, got 'stranger'"),
        (('withdraw', claimed, '--reason', ' '), 'a reason is required to withdraw a reminder'),
        (('send', claimed, '--text', ' '), 'reminder text must not be blank'),
        (('claim', 'h_00000000'), "unknown card 'h_00000000'"),
    ]:
        code, out, err = cli(capsys, 'reminder', *argv)
        assert (code, out, err) == (2, '', f'hive: reminder {argv[0]} refused: {error}\n'), argv
    with pytest.raises(SystemExit) as refused:
        cli(capsys, 'reminder', 'withdraw', claimed)
    assert refused.value.code == 2
    assert {cid: (api.get(conn, cid)['state'], api.get(conn, cid)['reminder_state']) for cid in before} == before
    assert tg_calls(tmp_path) == []


@pytest.mark.parametrize('kw,error', [
    ({'text': ' \n'}, 'reminder text must not be blank'),
    ({'text': 'x' * (32 * 1024 + 1)}, 'reminder text must be at most 32768 bytes of UTF-8'),
    ({'at': None}, 'a reminder needs not_before or a schedule'),
    ({'review': True}, 'a reminder runs no head'),
    ({'acceptance_kind': 'judge', 'judge_criteria': 'reminded'}, 'a reminder runs no head'),
    ({'recheck_every': 60, 'recheck_max': 2}, 'a reminder runs no head'),
])
def test_create_refuses_a_reminder_without_text_or_time_or_with_a_head(hive, kw, error):
    _, conn, _, _, _ = hive
    with pytest.raises(api.CardError, match=error):
        remind(conn, **kw)


def test_cli_creates_a_reminder_without_an_acceptance(hive, capsys):
    _, conn, _, _, _ = hive
    code, out, _ = cli(capsys, 'create', '--title', 'synthetic reminder', '--goal', 'remind the owner', '--step', '1',
                       '--source', 'system:test', '--created-by', 'secretary', '--kind', 'owner_task',
                       '--not-before', '2026-01-01T09:00:00+00:00', '--reminder', TEXT, '--json')
    assert code == 0
    card = api.get(conn, json.loads(out)['id'])
    assert (card['reminder'], card['state'], card['acceptance']) == (TEXT, 'created', 'true')


def test_each_instance_of_a_recurring_reminder_is_a_reminder(hive, tmp_path):
    _, conn, d, now, adapter = hive
    spawned = record_spawns(d)
    sid = mk_series(conn, every=600, max_runs=2, reminder=TEXT)
    at(d, now, START)
    at(d, now, START + 180)
    at(d, now, START + 600)
    at(d, now, START + 780, ticks=2)
    series = api.get(conn, sid)
    ids = [i['id'] for i in series['instances']]
    assert len(ids) == 2 and spawned == []
    assert [(p['card_id'], p['series'], p['period'], p['text']) for _, p in events(adapter, 'reminder_due')] == [
        (ids[0], sid, 1, TEXT), (ids[1], sid, 2, TEXT)]
    assert [api.get(conn, i)['reminder_state']['state'] for i in ids] == ['delivered', 'delivered']
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [TEXT, TEXT]
    assert series['state'] == 'done' and series['reminder_state'] is None


def test_due_and_claimed_reminders_wait_on_the_board(hive, capsys):
    _, conn, d, now, _ = hive
    due, claimed = remind(conn), remind(conn)
    at(d, now, START)
    assert cli(capsys, 'reminder', 'claim', claimed)[0] == 0
    waiting = {e['id']: e for e in api.board(conn, START + 10, alive=lambda run, root: False)['columns']['waiting']}
    assert {cid: (waiting[cid]['badge'], waiting[cid]['cause'], waiting[cid]['reason'], waiting[cid]['waiting_since'])
            for cid in (due, claimed)} == {due: ('external', 'reminder', 'due', START),
                                           claimed: ('external', 'reminder', 'claimed', START)}


def test_reminder_events_are_valid_c1_packets_for_the_owner_inbox(hive, tmp_path, c1):
    cfg, conn, _, now, _ = hive
    d = dispatcher.Dispatcher(dict(cfg, delivery='ulei', **ulei_keys(tmp_path, c1.path)), conn,
                              limits_fetcher=lambda c: {})
    d.clock = lambda: now[0]
    cid = remind(conn)
    at(d, now, START)
    at(d, now, START + 180, ticks=2)
    assert [(p['kind'], p['id'], json.loads(p['payload'])['text']) for p in c1.published()] == [
        ('reminder_due', f'hive:{cid}:reminder_due', TEXT), ('reminder_fallback', f'hive:{cid}:reminder_fallback', TEXT)]


@pytest.mark.parametrize('old,new,error', [
    ('tg_binary = "/srv/hive/bin/ulei-tg"\n', '', 'config: missing key reminders.tg_binary$'),
    ('tg_config = "/srv/hive/ulei.toml"\n', 'tg_config = ""\n', "config: reminders.tg_config must be a path, got ''$"),
    ('unclaimed_s = 180\n', 'unclaimed_s = 0\n', 'config: reminders.unclaimed_s must be a positive number, got 0$'),
    ('max_attempts = 20\n', 'max_attempts = 1.5\n', 'config: reminders.max_attempts must be a positive integer, got 1.5$'),
])
def test_reminders_table_is_checked_at_load(tmp_path, old, new, error):
    config = tmp_path / 'config.toml'
    text = EXAMPLE.read_text()
    assert text.count(old) == 1
    config.write_text(text.replace(old, new).replace('"texts/', f'"{EXAMPLE.parent}/texts/'))
    with pytest.raises(ValueError, match=error):
        store.load_config(config)


def test_reminder_timings_default_when_absent(tmp_path):
    config = tmp_path / 'config.toml'
    text = EXAMPLE.read_text()
    for line in ('unclaimed_s = 180\n', 'claim_silence_s = 300\n', 'retry_s = 60\n', 'max_attempts = 20\n',
                 'notify_timeout_s = 120\n'):
        assert text.count(line) == 1
        text = text.replace(line, '')
    config.write_text(text.replace('"texts/', f'"{EXAMPLE.parent}/texts/'))
    timings = {k: v for k, v in store.load_config(config)['reminders'].items() if not k.startswith('tg_')}
    assert timings == {'unclaimed_s': 180, 'claim_silence_s': 300, 'retry_s': 60, 'max_attempts': 20, 'notify_timeout_s': 120}
