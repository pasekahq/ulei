"""The delivery interface: the drop adapter's packets and readback, the ulei adapter's C1 calls on a fake inbox
socket, and the dispatcher and the steward handing events to the adapter config `delivery` names."""
import importlib.util
import json
from pathlib import Path

import pytest
from fake_c1_inbox import FakeC1Inbox
from fake_delivery import FakeAdapter
from fake_inbox import FakeInbox
from test_hive import FAKE_INBOX_LIBRARY, ROOT, env, mk  # noqa: F401
from hive import api, delivery, dispatcher, steward, store

GOLDEN = json.loads((Path(__file__).with_name('golden_c1_packets.json')).read_text(encoding='utf-8'))
KINDS = {'task_result', 'task_blocked', 'provider_outage', 'provider_recovery'}
KEY = GOLDEN['producer_key'].encode()


def drop(tmp_path):
    return delivery.Drop({'inbox_library': str(FAKE_INBOX_LIBRARY), 'inbox': str(tmp_path / 'inbox')})


@pytest.mark.parametrize('case', [pytest.param(c, id=c['name']) for c in GOLDEN['cases']])
def test_drop_adapter_stores_the_recorded_library_packet(tmp_path, case):
    adapter = drop(tmp_path)
    adapter.inbox = FakeInbox(GOLDEN['producer_key'].encode())
    args = case['publish_args']
    event = delivery.Event(args['kind'], args['event_id'], args['payload'], args['source_ts'])
    assert not adapter.delivered(event.id)
    adapter.deliver(event)
    assert adapter.delivered(event.id)
    assert adapter.inbox.packets == {event.id: case['packet']}
    assert adapter.inbox.get(event.id) == {'id': event.id, 'packet': case['stored']['packet'].encode(),
                                           'digest': case['stored']['digest']}


class Forgetful(FakeInbox):
    """Accepts every packet and never returns one."""

    def get(self, event_id):
        return None


def test_drop_readback_failure_stays_on_the_outbox_row_until_a_later_publish(env):
    _, conn, d = env
    cid = mk(conn, 'readback')
    with store.tx(conn):
        d.finish(api.card_row(conn, cid), 'done', 'gate passed')
    good = d.adapter().inbox
    d.adapter().inbox = Forgetful()
    assert d.publish() == 0
    [row] = api.get(conn, cid)['outbox']
    assert row['published_at'] is None and row['error'] == 'RuntimeError: inbox readback failed'
    d.adapter().inbox = good
    assert d.publish() == 1
    assert d.adapter().delivered(row['event_id'])


def outbox(conn):
    return {r['event_id']: r for r in conn.execute('SELECT * FROM outbox').fetchall()}


def test_dispatcher_hands_each_event_kind_to_the_configured_adapter(env, monkeypatch):
    cfg, conn, _ = env
    adapter = FakeAdapter()
    monkeypatch.setitem(delivery.ADAPTERS, 'fake', lambda c: adapter)
    d = dispatcher.Dispatcher(dict(cfg, delivery='fake'), conn, limits_fetcher=lambda c: {})
    done, blocked = mk(conn, 'done'), mk(conn, 'blocked')
    with store.tx(conn):
        d.finish(api.card_row(conn, done), 'done', 'gate passed')
        d.finish(api.card_row(conn, blocked), 'blocked', 'synthetic failure')
        d.queue_provider_episode(blocked, 'e1', 'outage', ['claude'])
        d.queue_provider_episode(blocked, 'e1', 'recovery', ['claude'])
    rows = outbox(conn)
    assert d.publish() == 4
    assert {e.kind for e in adapter.events} == KINDS
    assert {e.id: (e.kind, e.source_ts) for e in adapter.events} == {
        i: (r['kind'], r['source_ts']) for i, r in rows.items()}
    for e in adapter.events:
        payload = json.loads(e.payload)
        assert payload.items() >= json.loads(rows[e.id]['payload']).items()
        card = api.card_row(conn, rows[e.id]['card_id'])
        assert (payload['card_class'], payload['owner_visible']) == (card['card_class'], bool(card['owner_visible']))
    assert all(r['published_at'] is not None and r['error'] is None for r in outbox(conn).values())
    assert d.publish() == 0 and len(adapter.events) == 4


def test_adapter_failure_is_recorded_on_the_outbox_row(env, monkeypatch):
    cfg, conn, _ = env
    adapter = FakeAdapter(OSError('secretary unreachable'))
    monkeypatch.setitem(delivery.ADAPTERS, 'fake', lambda c: adapter)
    d = dispatcher.Dispatcher(dict(cfg, delivery='fake'), conn, limits_fetcher=lambda c: {})
    cid = mk(conn, 'unreachable')
    with store.tx(conn):
        d.finish(api.card_row(conn, cid), 'blocked', 'synthetic failure')
    assert d.publish() == 0
    [row] = outbox(conn).values()
    assert row['published_at'] is None and row['error'] == 'OSError: secretary unreachable'
    adapter.error = None
    assert d.publish() == 1
    assert [e.id for e in adapter.events] == [row['event_id']]


def ulei_keys(tmp_path, socket_path):
    """The ulei adapter's config keys: `socket_path` and an owner-only file holding KEY."""
    key_file = tmp_path / 'producer.key'
    key_file.write_bytes(KEY)
    key_file.chmod(0o600)
    return {'inbox_socket': str(socket_path), 'inbox_producer_key_file': str(key_file)}


@pytest.fixture
def c1(tmp_path):
    """The fake C1 inbox on `c1.sock`, signing with KEY; every request it got must be a valid C1 request."""
    server = FakeC1Inbox(tmp_path / 'c1.sock', KEY)
    yield server
    server.close()
    assert server.invalid == []


def ulei_dispatcher(env, tmp_path, socket_path):
    cfg, conn, _ = env
    return dispatcher.Dispatcher(dict(cfg, delivery='ulei', **ulei_keys(tmp_path, socket_path)), conn,
                                 limits_fetcher=lambda c: {})


@pytest.mark.parametrize('case', [pytest.param(c, id=c['name']) for c in GOLDEN['cases']])
def test_ulei_publishes_the_packet_bytes_and_digest_of_drop(tmp_path, c1, case):
    ulei = delivery.Ulei(ulei_keys(tmp_path, c1.path))
    dropped = drop(tmp_path)
    dropped.inbox = FakeInbox(KEY)
    args = case['publish_args']
    event = delivery.Event(args['kind'], args['event_id'], args['payload'], args['source_ts'])
    assert not ulei.delivered(event.id)
    ulei.deliver(event)
    assert ulei.delivered(event.id)
    dropped.deliver(event)
    stored = case['stored']['packet'].encode()
    assert [r for r in c1.requests if r[0] == '/v1/publish'] == [('/v1/publish', b'{"packet":' + stored + b'}')]
    assert c1.inbox.get(event.id) == dropped.inbox.get(event.id) == {
        'id': event.id, 'packet': stored, 'digest': case['stored']['digest']}


def test_ulei_conflict_stays_on_the_outbox_row_until_a_later_publish(env, tmp_path, c1):
    _, conn, _ = env
    d = ulei_dispatcher(env, tmp_path, c1.path)
    cid = mk(conn, 'conflict')
    with store.tx(conn):
        d.finish(api.card_row(conn, cid), 'done', 'gate passed')
    [row] = outbox(conn).values()
    c1.inbox.publish(c1.inbox.packet(row['kind'], row['event_id'], 'different content', row['source_ts']))
    assert d.publish() == 0
    [row] = outbox(conn).values()
    assert row['published_at'] is None
    assert row['error'] == 'C1Error: publish: 409 conflict: event id already has different content'
    c1.inbox = FakeInbox(KEY)
    assert d.publish() == 1
    assert d.adapter().delivered(row['event_id'])
    assert [p['id'] for p in c1.published()] == [row['event_id']] * 2


def test_ulei_missing_socket_stays_on_the_outbox_row_until_a_later_publish(env, tmp_path):
    _, conn, _ = env
    path = tmp_path / 'later.sock'
    d = ulei_dispatcher(env, tmp_path, path)
    cid = mk(conn, 'no socket')
    with store.tx(conn):
        d.finish(api.card_row(conn, cid), 'blocked', 'synthetic failure')
    assert d.publish() == 0
    [row] = outbox(conn).values()
    assert row['published_at'] is None
    assert row['error'] == 'FileNotFoundError: [Errno 2] No such file or directory'
    server = FakeC1Inbox(path, KEY)
    try:
        assert d.publish() == 1
        assert d.adapter().delivered(row['event_id'])
    finally:
        server.close()
    assert [p['id'] for p in server.published()] == [row['event_id']] and server.invalid == []


@pytest.mark.parametrize('adapter', ['drop', 'ulei'])
def test_dispatcher_switches_adapter_by_the_config_key_only(env, tmp_path, c1, adapter):
    _, conn, _ = env
    config = tmp_path / 'config.toml'
    keys = ''.join(f'{k} = "{v}"\n' for k, v in ulei_keys(tmp_path, c1.path).items())
    config.write_text(config.read_text().replace('delivery = "drop"\n', f'delivery = "{adapter}"\n{keys}'))
    cfg = store.load_config(config)
    d = dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {})
    done, blocked = mk(conn, 'done'), mk(conn, 'blocked')
    with store.tx(conn):
        d.finish(api.card_row(conn, done), 'done', 'gate passed')
        d.finish(api.card_row(conn, blocked), 'blocked', 'synthetic failure')
    rows = {i: r['kind'] for i, r in outbox(conn).items()}
    assert d.publish() == 2 and set(rows.values()) == {'task_result', 'task_blocked'}
    sinks = {'drop': delivery.Drop(cfg).inbox.packets, 'ulei': c1.inbox.packets}
    assert {name: {i: p['kind'] for i, p in sink.items()} for name, sink in sinks.items()} == {
        name: rows if name == adapter else {} for name in sinks}


def test_steward_alert_through_ulei_is_sent_once_per_error(env, tmp_path, c1, monkeypatch, capsys):
    config = tmp_path / 'config.toml'
    keys = ''.join(f'{k} = "{v}"\n' for k, v in ulei_keys(tmp_path, c1.path).items())
    config.write_text(config.read_text().replace('delivery = "drop"\n', f'delivery = "ulei"\n{keys}'))
    spec = importlib.util.spec_from_file_location('steward_ulei', ROOT / 'scripts/hive_steward.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)

    class Morning:
        @classmethod
        def now(cls, tz):
            from datetime import datetime
            return datetime(2026, 11, 10, 7, 30, tzinfo=tz)
    monkeypatch.setattr(module, 'datetime', Morning)

    def broken(*a):
        raise steward.ModelResponseError('empty model answer after a JSON-only retry', '')
    monkeypatch.setattr(steward, 'query_model', broken)
    for _ in range(2):
        assert module.main(['--apply', '--config', str(config), '--output', str(tmp_path / 'steward')]) == 0
        assert json.loads(capsys.readouterr().out.splitlines()[-1])['alerted'] is True
    [packet] = c1.published()
    assert packet['id'].startswith('hive-steward-error:2026-11-10:') and packet['kind'] == 'task_result'
    assert 'ModelResponseError: empty model answer' in packet['payload']
    assert c1.inbox.packets == {packet['id']: packet}
