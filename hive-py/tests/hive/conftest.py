"""The hive-py root on sys.path, so `hive` and `scripts` import from any working directory; the fake qb gateway every
test config names."""
import sys
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from fake_gateway import FakeGateway  # noqa: E402


@pytest.fixture
def gateway(tmp_path):
    """The scripted gateway on `gw.sock` in the test's directory; every request it got must be a valid TurnRequest."""
    gw = FakeGateway(tmp_path / 'gw.sock')
    yield gw
    gw.close()
    assert gw.invalid == []
