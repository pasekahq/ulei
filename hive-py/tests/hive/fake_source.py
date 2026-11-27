"""An outside source of the watchdog for the hive tests (its journal or escalation command): `fake_source.py DIR ARGS...`.

Every call is appended to DIR/calls.jsonl as its argv. It prints DIR/stdout when present and exits with the number in
DIR/exit when present, else 0."""
import json
import sys
from pathlib import Path


def main(argv):
    root = Path(argv[0])
    with (root / 'calls.jsonl').open('a') as f:
        f.write(json.dumps(argv[1:]) + '\n')
    out = root / 'stdout'
    if out.exists():
        sys.stdout.write(out.read_text())
    code = root / 'exit'
    return int(code.read_text()) if code.exists() else 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
