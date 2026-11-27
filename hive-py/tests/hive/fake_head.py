#!/usr/bin/env python3
"""Fake head for Hive tests: behaves like `claude -p` / `codex exec` reading the prompt on stdin.

Behaviour is chosen by tokens in the card title: [ok] [never] [round2] [crash] [exit1]
[reviewfail] [reviewround2] [sleep]. Writes REPORT.md / REVIEW.md in cwd.
A worker whose cwd has a `hold-r<round>` FIFO writes work-r<round>.txt, writes its round into the
`ready-r<round>` FIFO and blocks until a byte is written to `hold-r<round>`. The report lists the work-*
files it found and copies the prompt's owner clarifications verbatim, except in a round named by
[noquote-r<round>]; [noreport-r<round>] writes no REPORT.md in that round; [placeholder-r<round>] opens that
round's report with an unfinished-placeholder RESULT line.
"""
import json
import os
import re
import sys
import time

prompt = sys.stdin.read()
m = re.search(r'# Card (\S+): (.*)', prompt)
title = m.group(2) if m else ''
rnd = int(re.search(r'Round (\d+)', prompt).group(1)) if re.search(r'Round (\d+)', prompt) else 1
reviewer = prompt.startswith('You are an independent REVIEWER')
with open(os.path.join(os.getcwd(), 'calls.log'), 'a') as f:
    f.write(json.dumps({'argv': sys.argv[1:], 'reviewer': reviewer, 'round': rnd}) + '\n')
if not reviewer and os.path.exists(f'hold-r{rnd}'):
    open(f'work-r{rnd}.txt', 'w').write(f'draft of round {rnd}\n')
    hold = os.open(f'hold-r{rnd}', os.O_RDWR)  # open before ready, read after: a release byte cannot be missed
    with open(f'ready-r{rnd}', 'w') as f:
        f.write(str(rnd))
    os.read(hold, 1)
if '[sleep]' in title:
    time.sleep(30)
if '[crash-once]' in title and not reviewer and not os.path.exists('crashed-once'):
    open('crashed-once', 'w').write('1')
    os.kill(os.getppid(), 9)
    time.sleep(5)
    sys.exit(9)
if '[crash]' in title and not reviewer:
    os.kill(os.getppid(), 9)  # kill the runner: no final status is written
    time.sleep(5)
    sys.exit(9)
if '[exit1]' in title and not reviewer:
    print(json.dumps({'type': 'result', 'result': 'task failed', 'usage': {'output_tokens': 3}}))
    sys.exit(1)
if '[provider429]' in title and not reviewer and not os.path.exists('provider-failed'):
    open('provider-failed', 'w').write('1')
    print('HTTP 429: too many requests', file=sys.stderr)
    sys.exit(1)
if '[provideroutage]' in title and not reviewer and len(open('calls.log').readlines()) <= 2:
    print('HTTP 429: too many requests', file=sys.stderr)
    sys.exit(1)
if reviewer:
    fail = '[reviewfail]' in title or ('[reviewround2]' in title and '(round 1)' in prompt)
    notes = '## Notes\nminor wording\n' if '[reviewnotes]' in title else ''
    open('REVIEW.md', 'w').write(('VERDICT: FAIL\nmissing thing\n' if fail else 'VERDICT: PASS\nlooks right\n') + notes)
else:
    write = '[ok]' in title or ('[round2]' in title and rnd >= 2) or '[reviewfail]' in title or '[reviewround2]' in title
    if write and '[never]' not in title:
        open('out.txt', 'w').write('42\n')
    found = sorted(n for n in os.listdir('.') if n.startswith('work-'))
    quote = '## Owner clarifications' in prompt and f'[noquote-r{rnd}]' not in title
    clarified = prompt[prompt.index('## Owner clarifications'):] if quote else ''
    if f'[noreport-r{rnd}]' not in title:
        report_file = re.search(r'When done, write (\S+) here', prompt).group(1)
        members = re.findall(r'^### Member (\S+)', prompt, re.MULTILINE)
        stub = 'RESULT: in progress, placeholder, overwritten on finish\n' if f'[placeholder-r{rnd}]' in title else ''
        open(report_file, 'w').write(stub + f'round {rnd} done\nfound: {" ".join(found)}\n{clarified}'
                                     + ('\nMembers: ' + ', '.join(members) if members else ''))
print(json.dumps({'type': 'result', 'result': 'fake', 'usage': {'input_tokens': 10, 'output_tokens': 3}, 'modelUsage': {'fake': {}}}))
