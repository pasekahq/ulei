"""GitHub workflow annotations of the CI drivers, which the public check-runs API serves without
the job log. GitHub keeps at most 10 notices per step and 50 annotations per job."""

from __future__ import annotations

import subprocess
from collections.abc import Callable

from inbox_load import Failure

# Workflow command escapes: the data escapes `%` and line ends, a property also `:` and `,`.
DATA = (("%", "%25"), ("\r", "%0D"), ("\n", "%0A"))
PROPERTY = (*DATA, (":", "%3A"), (",", "%2C"))


def escape(text: str, escapes: tuple[tuple[str, str], ...]) -> str:
    for raw, escaped in escapes:
        text = text.replace(raw, escaped)
    return text


def annotate(level: str, title: str, message: str) -> None:
    """One annotation of ``level`` with ``title`` and ``message``."""
    print(f"::{level} title={escape(title, PROPERTY)}::{escape(message, DATA)}", flush=True)


class Notices:
    """The passed checks of one step, gathered under a few notice titles and written at its
    end; a failed check is written at once as an error under its own title."""

    def __init__(self) -> None:
        self.folds: dict[str, list[str]] = {}

    def add(self, fold: str, title: str, message: str) -> None:
        self.folds.setdefault(fold, []).append(f"{title}: {message}")

    def check(self, fold: str, title: str, probe: Callable[[], str]) -> bool:
        try:
            message = probe()
        except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
            annotate("error", title, str(e))
            return False
        self.add(fold, title, message)
        return True

    def pair(
        self, fold: str, titles: tuple[str, str], probe: Callable[[], tuple[str, str]],
    ) -> bool:
        """One check that yields two texts, one line under each of ``titles``."""
        try:
            first, second = probe()
        except (Failure, OSError, subprocess.SubprocessError, KeyError) as e:
            annotate("error", titles[0], str(e))
            return False
        self.add(fold, titles[0], first)
        self.add(fold, titles[1], second)
        return True

    def write(self) -> None:
        for fold, lines in self.folds.items():
            annotate("notice", fold, "\n".join(lines))
