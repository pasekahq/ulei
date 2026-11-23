"""The one loader of the deployment's owner-inbox library: the directory config `inbox_library` names."""
import sys
from pathlib import Path


def open_inbox(cfg):
    """The library's `Inbox` on the inbox directory config `inbox` names."""
    library = str(cfg['inbox_library'])
    if library not in sys.path:
        sys.path.insert(0, library)
    from inbox import Inbox
    return Inbox(Path(cfg['inbox']))
