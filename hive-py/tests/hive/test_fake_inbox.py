"""The fake inbox against the golden packets the real inbox library made at `SOURCE`, and C1."""
import copy
import json
from pathlib import Path

import pytest
from fake_inbox import FakeInbox
from schema_check import violations

HERE = Path(__file__).resolve().parent
GOLDEN = json.loads((HERE / 'golden_c1_packets.json').read_text(encoding='utf-8'))
C1 = json.loads((HERE.parents[2] / 'contracts' / 'c1-event.schema.json').read_text(encoding='utf-8'))
KEY = GOLDEN['producer_key'].encode()
CASES = [pytest.param(c, id=c['name']) for c in GOLDEN['cases']]


@pytest.mark.parametrize('case', CASES)
def test_fake_packet_is_the_recorded_library_packet(case):
    packet = FakeInbox(KEY).packet(**case['publish_args'])
    assert packet == case['packet']


@pytest.mark.parametrize('case', CASES)
def test_publish_stores_the_recorded_bytes_and_get_returns_them(case):
    inbox = FakeInbox(KEY)
    event_id = case['publish_args']['event_id']
    assert inbox.get(event_id) is None
    assert inbox.publish(inbox.packet(**case['publish_args'])) == event_id
    row = inbox.get(event_id)
    assert row == {'id': event_id, 'packet': case['stored']['packet'].encode(), 'digest': case['stored']['digest']}
    assert inbox.packets == {event_id: case['packet']}


def test_publish_dedupes_one_id_and_refuses_other_content_under_it():
    inbox = FakeInbox(KEY)
    args = GOLDEN['cases'][0]['publish_args']
    inbox.publish(inbox.packet(**args))
    inbox.publish(inbox.packet(**args))
    assert list(inbox.packets) == [args['event_id']]
    with pytest.raises(ValueError, match='different content'):
        inbox.publish(inbox.packet(**dict(args, payload='{"state": "other"}')))


def test_publish_refuses_a_packet_under_another_key():
    packet = FakeInbox(KEY).packet(**GOLDEN['cases'][0]['publish_args'])
    with pytest.raises(ValueError, match='unauthenticated'):
        FakeInbox().publish(packet)


@pytest.mark.parametrize('case', CASES)
def test_recorded_packet_is_a_valid_c1_event(case):
    assert violations(case['packet'], C1, C1) == []
    assert violations(json.loads(case['stored']['packet']), C1, C1) == []


def test_c1_check_refuses_a_packet_off_the_contract():
    packet = copy.deepcopy(GOLDEN['cases'][0]['packet'])
    packet.update(version=2, id='bad id', extra=1)
    del packet['mac']
    assert set(violations(packet, C1, C1)) == {
        '$: missing mac', '$.version: maximum 1', '$.id: does not match ^[a-zA-Z0-9:_-]{1,96}$', '$: unexpected extra'}
