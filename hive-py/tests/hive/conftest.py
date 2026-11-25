"""The hive-py root on sys.path, so `hive` and `scripts` import from any working directory; the fake qb gateway every
test config names."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from fake_gateway import FakeGateway  # noqa: E402
import journal_watch  # noqa: E402


@pytest.fixture
def gateway(tmp_path):
    """The scripted gateway on `gw.sock` in the test's directory; every request it got must be a valid TurnRequest."""
    gw = FakeGateway(tmp_path / 'gw.sock')
    yield gw
    gw.close()
    assert gw.invalid == []


@pytest.fixture(autouse=True)
def journal_contract(monkeypatch):
    """Every store connection of the test keeps the board's journal contract (see journal_watch.py)."""
    from hive import store
    opened = []
    connect = store.connect

    def watched(*args, **kwargs):
        conn = journal_watch.install(connect(*args, **kwargs))
        opened.append(conn)
        return conn
    monkeypatch.setattr(store, 'connect', watched)
    monkeypatch.setattr(store, 'tx', journal_watch.checked_tx(store.tx))
    journal_watch.BREACHES.clear()
    yield
    for conn in opened:
        journal_watch.check(conn, 'end of test')
    assert journal_watch.BREACHES == []
