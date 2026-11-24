"""The owner-inbox library the tests' config `inbox_library` names: one FakeInbox per inbox directory."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from fake_inbox import FakeInbox  # noqa: E402

_OPENED = {}


def Inbox(root):  # noqa: N802 - the library's constructor name
    return _OPENED.setdefault(str(Path(root)), FakeInbox())
