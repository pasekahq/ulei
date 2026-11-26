"""A delivery adapter for the tests: keeps every event it is handed, or refuses each with `error`."""


class FakeAdapter:
    def __init__(self, error=None):
        self.error = error
        self.events = []

    def deliver(self, event):
        if self.error is not None:
            raise self.error
        self.events.append(event)

    def delivered(self, event_id):
        return any(e.id == event_id for e in self.events)

    def status(self, event_id):
        """The owner inbox row a snapshot shows: `done` with outcome `success` once handed over, else None."""
        return {'status': 'done', 'attempts': 1, 'outcome': 'success'} if self.delivered(event_id) else None
