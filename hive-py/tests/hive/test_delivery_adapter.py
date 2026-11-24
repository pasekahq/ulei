"""The delivery interface: the drop adapter's packets and readback, and the dispatcher handing every event kind
to the adapter config `delivery` names."""
import json
from pathlib import Path

import pytest
from fake_delivery import FakeAdapter
from fake_inbox import FakeInbox
from test_hive import FAKE_INBOX_LIBRARY, env, mk  # noqa: F401
from hive import api, delivery, dispatcher, store

GOLDEN = json.loads((Path(__file__).with_name('golden_c1_packets.json')).read_text(encoding='utf-8'))
KINDS = {'task_result', 'task_blocked', 'provider_outage', 'provider_recovery'}


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
