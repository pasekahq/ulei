"""The one way hive events reach the secretary: `open_adapter(cfg)` gives the adapter config `delivery` names."""
import sys
from pathlib import Path
from typing import NamedTuple


class Event(NamedTuple):
    """One event for the secretary: a C1 kind, its id, its JSON text payload and the source time."""
    kind: str
    id: str
    payload: str
    source_ts: float


class Drop:
    """Publishes into the owner inbox through the deployment's library: the module `inbox` in the directory
    config `inbox_library` names, its `Inbox` on the directory config `inbox` names."""

    def __init__(self, cfg):
        library = str(cfg['inbox_library'])
        if library not in sys.path:
            sys.path.insert(0, library)
        from inbox import Inbox
        self.inbox = Inbox(Path(cfg['inbox']))

    def deliver(self, event):
        """Publish `event` and read it back; raises when the inbox refuses it or does not return it."""
        self.inbox.publish(self.inbox.packet(event.kind, event.id, event.payload, event.source_ts))
        if self.inbox.get(event.id) is None:
            raise RuntimeError('inbox readback failed')

    def delivered(self, event_id):
        return self.inbox.get(event_id) is not None


ADAPTERS = {'drop': Drop}
# The adapter of C1 `publish` on `inbox.socket`; a config naming it is refused until it is in ADAPTERS.
PLANNED = ('ulei',)


def check(cfg):
    """Refuse a config whose `delivery` names no adapter."""
    name = cfg['delivery']
    if name in PLANNED:
        raise ValueError(f'config: delivery {name!r} has no adapter yet; adapters: {", ".join(ADAPTERS)}')
    if not isinstance(name, str) or name not in ADAPTERS:
        raise ValueError(f'config: delivery must be one of {", ".join(ADAPTERS)}, got {name!r}')


def open_adapter(cfg):
    """The adapter config `delivery` names: `deliver(event)` and `delivered(event_id)`."""
    return ADAPTERS[cfg['delivery']](cfg)
