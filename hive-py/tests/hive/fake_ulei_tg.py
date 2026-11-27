"""`ulei-tg notify` for the hive tests: `fake_ulei_tg.py DIR notify --config PATH`, text on stdin.

Every call is appended to DIR/calls.jsonl as {"argv", "stdin"}. DIR/exits.json, when present, is a list of answers
used one per call, then 0: an exit code, or "no-receipt" for exit 0 with a receipt of no chats. Exit 0 prints the receipt
line of one chat with message id 100 + the call's number; any other exit prints one stderr line."""
import json
import sys
from pathlib import Path

CHAT_ID = 4242


def main(argv):
    root = Path(argv[0])
    calls = root / 'calls.jsonl'
    number = len(calls.read_text().splitlines()) + 1 if calls.exists() else 1
    with calls.open('a') as f:
        f.write(json.dumps({'argv': argv[1:], 'stdin': sys.stdin.buffer.read().decode()}) + '\n')
    exits_file = root / 'exits.json'
    exits = json.loads(exits_file.read_text()) if exits_file.exists() else []
    code = exits.pop(0) if exits else 0
    exits_file.write_text(json.dumps(exits))
    if code == 'no-receipt':
        print(json.dumps({'chats': []}))
        return 0
    if code != 0:
        print(f'ulei-tg notify: sent 0 of 1 messages, then telegram network: synthetic failure {number}', file=sys.stderr)
        return code
    print(json.dumps({'chats': [{'chat_id': CHAT_ID, 'message_ids': [100 + number]}]}))
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
