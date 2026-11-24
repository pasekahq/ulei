"""Timezone-aware cron cursor. Persist the next UTC instant; never rebuild it from wall time."""
import re
from datetime import datetime, time, timezone
from zoneinfo import ZoneInfo

from cronsim import CronSim


def parse_start_window(value):
    """A local, same-day half-open interval with minute precision."""
    if not isinstance(value, str) or not re.fullmatch(r'[0-2][0-9]:[0-5][0-9]-[0-2][0-9]:[0-5][0-9]', value):
        raise ValueError('start_window must be HH:MM-HH:MM')
    start, end = (time.fromisoformat(part) for part in value.split('-'))
    if start >= end:
        raise ValueError('start_window must end after it starts on the same day')
    return start, end


def occurrence_window(value, tz, occurrence):
    """Absolute bounds on the occurrence's local date; gaps shift forward, folds use the first instant."""
    start, end = parse_start_window(value)
    zone = ZoneInfo(tz)
    day = datetime.fromtimestamp(occurrence, zone).date()
    return (datetime.combine(day, start, zone).timestamp(),
            datetime.combine(day, end, zone).timestamp())


def same_local_day(tz, a, b):
    """True when two instants fall on the same calendar date in the series timezone."""
    zone = ZoneInfo(tz)
    return datetime.fromtimestamp(a, zone).date() == datetime.fromtimestamp(b, zone).date()


def next_cron(expr, tz, after):
    """Next occurrence strictly after an instant, including Debian's DST rules.

    CronSim can return fold=0 before `after` when restarted inside fold=1 for a
    fixed-time expression. Filter by timestamp so a repeated hour never reruns it.
    """
    it = CronSim(expr, datetime.fromtimestamp(after, timezone.utc).astimezone(ZoneInfo(tz)))
    for value in it:
        stamp = value.timestamp()
        if stamp > after:
            return stamp
    raise ValueError('cron has no occurrence in the next 50 years')


def due_cron(series, now):
    """(latest ordinal, latest due instant, next instant), or None. No historical replay.

    Ordinals count all occurrences, including skipped ones. Work is bounded per
    tick for a cursor that has been offline for years; it advances in batches.
    """
    due = series['series_due']
    if due > now:
        return None
    period = series['series_next']
    for _ in range(100000):
        previous = due
        following = next_cron(series['series_cron'], series['series_tz'], due)
        if following > now:
            return period, due, following
        period, due = period + 1, following
    return period - 1, previous, due
