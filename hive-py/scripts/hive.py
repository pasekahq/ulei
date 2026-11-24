#!/usr/bin/env python3
"""Hive (Улей) CLI. The config comes from --config or HIVE_CONFIG.

  hive.py create --title T --goal G (--acceptance 'CMD' | --accept-review 'CRITERIA') --step 1..5 --source S --created-by secretary
                 [--review] [--body B] [--type T] [--idem KEY] [--hold | --not-before TIMESTAMP]
                 [--recheck-every DURATION] [--recheck-until TIMESTAMP] [--recheck-max COUNT] [--blocked-by CARD ...]
                 [--epic | --parent EPIC [--optional]] [--assignee EXECUTOR [--on-unavailable wait|fallback]]
                 [--model ID [--effort E]] [--executor NAME [--on-unavailable wait|fallback]]
                 [--every DURATION | --cron EXPR [--tz IANA] [--misfire skip|latest] [--start-window HH:MM-HH:MM]]
                 [--max-runs N] [--until TIMESTAMP] [--overlap skip|queue]
                 [--script PATH [--script-timeout SECONDS]] [--deliver telegram|owner-inbox|local]
                 [--wait SECONDS] [--json]
  hive.py list [--state S ...] [--json]
  hive.py board [--json] [--archive [--limit N]]   columns in progress, waiting (with a badge) and done;
                 --archive adds the archived cards, newest first
  hive.py show CARD [--json] [--prompts]
  hive.py release CARD      lift a not_before hold (trigger happened)
  hive.py cancel CARD [--reason TEXT] [--by OWNER|secretary]   cancel and wait for active heads to exit;
                 OWNER is config owner_actor
  hive.py stop CARD [--by ACTOR]   a recurring card creates no further instances; existing ones run on
  hive.py assign CARD (EXECUTOR | --none) [--on-unavailable wait|fallback] [--by ACTOR]
                 set or remove the worker executor (a [vendors.*] name) of a created, queued or waiting card
  hive.py depend CARD BLOCKER [--by ACTOR]     CARD waits in blocked until BLOCKER is done
  hive.py undepend CARD BLOCKER [--by ACTOR]   drop that dependency; the dispatcher unblocks CARD on its tick
  hive.py attach CARD EPIC [--optional] [--by ACTOR]   CARD becomes a child of EPIC (required unless --optional)
  hive.py detach CARD EPIC [--by ACTOR]                CARD leaves EPIC
  hive.py steer CARD TEXT [--by ACTOR]   deliver a clarification into the live worker session;
                 restart only if unavailable; `show` prints delivery and the transport method
  hive.py tick              one dispatcher pass (the systemd service runs `daemon`)
  hive.py daemon [--interval 10]
  hive.py observe           anomalies JSON for the 30-min observer cron (exit 0 always)
  hive.py ladder            the step -> heads table from the config
"""
import argparse
import datetime as dt
import json
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from hive import api, store, dispatcher, heads, liveness  # noqa: E402

TZ = dt.timezone(dt.timedelta(hours=1))


def ts(v, tz):
    if not v:
        return '—'
    from zoneinfo import ZoneInfo
    return dt.datetime.fromtimestamp(v, ZoneInfo(tz)).strftime('%d.%m %H:%M:%S')


def parse_deny_tools(value):
    try:
        return api.normalize_deny_tools(value.split(','))
    except api.CardError as e:
        raise argparse.ArgumentTypeError(str(e)) from e


def parse_duration(value):
    match = re.fullmatch(r'([1-9][0-9]*)([smhd])', value)
    if match is None:
        raise argparse.ArgumentTypeError('must be a positive Ns, Nm, Nh, or Nd duration')
    return int(match.group(1)) * {'s': 1, 'm': 60, 'h': 3600, 'd': 86400}[match.group(2)]


def parse_timestamp(value):
    try:
        parsed = dt.datetime.fromisoformat(value)
    except ValueError as e:
        raise argparse.ArgumentTypeError('must be an offset-aware ISO-8601 timestamp') from e
    if parsed.tzinfo is None or parsed.utcoffset() is None:
        raise argparse.ArgumentTypeError('must be an offset-aware ISO-8601 timestamp')
    return parsed.timestamp()


def main(argv=None):
    ap = argparse.ArgumentParser(prog='hive.py')
    ap.add_argument('--config')
    sub = ap.add_subparsers(dest='cmd', required=True)
    c = sub.add_parser('create')
    c.add_argument('--title', required=True)
    c.add_argument('--goal', required=True)
    acceptance = c.add_mutually_exclusive_group()
    acceptance.add_argument('--acceptance', help='bash command; exit 0 = accepted')
    acceptance.add_argument('--accept-review', help='review criteria; REPORT.md must be non-empty')
    c.add_argument('--step', type=int, required=True, choices=range(1, 6))
    c.add_argument('--kind', choices=api.CLASSES, dest='card_class')
    c.add_argument('--owner-visible', action=argparse.BooleanOptionalAction, default=None)
    c.add_argument('--followup-of')
    c.add_argument('--source', required=True)
    c.add_argument('--created-by', required=True)
    c.add_argument('--review', action='store_true', help='independent review by the other vendor (off by default)')
    c.add_argument('--deny-tools', type=parse_deny_tools, default=(),
                   help='comma-separated tool denials: browser,network,write')
    c.add_argument('--body', default='')
    c.add_argument('--body-file')
    c.add_argument('--type', dest='task_type', default='general')
    c.add_argument('--idem', dest='idem_key')
    c.add_argument('--gate-timeout', type=float, help='acceptance timeout in seconds')
    c.add_argument('--priority', type=int, default=None)
    schedule = c.add_mutually_exclusive_group()
    schedule.add_argument('--hold', action='store_true', help='park until `hive.py release` (card waits for a trigger)')
    schedule.add_argument('--not-before', type=parse_timestamp, help='offset-aware ISO-8601 time before which the card is held')
    c.add_argument('--recheck-every', type=parse_duration, help='positive Ns, Nm, Nh, or Nd recheck interval')
    c.add_argument('--recheck-until', type=parse_timestamp, help='offset-aware ISO-8601 recheck deadline')
    c.add_argument('--recheck-max', type=int, help='positive maximum number of rechecks')
    c.add_argument('--blocked-by', action='append', default=[], help='card id this card waits for (repeatable)')
    tree = c.add_mutually_exclusive_group()
    tree.add_argument('--epic', action='store_true', help='a container card: no heads, done when its required children are done and its acceptance passes')
    tree.add_argument('--parent', help='epic id this card is a child of')
    c.add_argument('--optional', action='store_true', help='with --parent: the epic does not wait for this child')
    c.add_argument('--assignee', help='worker executor: a [vendors.*] name the dispatcher can run')
    c.add_argument('--model', help='worker model id from the loaded ladder; implies its vendor as --assignee, '
                   'refuses a conflicting assignee or an epic; recurring instances copy the pin')
    c.add_argument('--effort', help='requires --model; an effort used by its ladder vendor; defaults to that '
                   'model at this step, otherwise required; reviewer keeps the other vendor and ladder head')
    c.add_argument('--panel', nargs='?', const=True,
                   help='independent members: model[:effort],model[:effort]; without a list, one ladder head per '
                   'configured runnable vendor at this step. Needs two distinct members across two vendors. '
                   'Each uses a slot and vendor budget, waits without substitution, and writes REPORT-<label>.md '
                   '(model name without vendor prefix or version, e.g. REPORT-opus.md, REPORT-astra.md; '
                   'collisions add effort, then a stable numeric suffix). '
                   'After all reports, a ladder synthesis head uses a slot and vendor budget to produce deliverables '
                   'and REPORT.md in the card workspace, where acceptance runs. REPORT.md must name every label. '
                   'Member or synthesis failure retries only that head; gate failure retries all members then synthesis. '
                   'Refuses model, effort, assignee, review, epic, recurring and steer')
    c.add_argument('--executor', help='external executor from [executors.*] (e.g. dots) instead of a head: the dispatcher '
                   'sends the card once per round when admitted (cooldown, max_parallel, daily_sends), polls for the '
                   'answer without a slot and gates its report; refuses assignee, model, effort, panel, epic, '
                   'recurring and a task type listed in its refuse_task_types')
    c.add_argument('--on-unavailable', default='wait',
                   help='wait (default) for the assignee, including provider failures, or fallback to the other '
                   'vendor\'s ladder head; model pins journal both models and never substitute within their vendor. '
                   'With --executor: wait while it is not admitted, or fallback to a ladder head when refused and '
                   'for the round after an executor timeout')
    cadence = c.add_mutually_exclusive_group()
    cadence.add_argument('--cron', help='five-field cron expression; unbounded unless max-runs or until is set')
    c.add_argument('--tz', help='IANA time zone for cron (default: config owner_tz)')
    c.add_argument('--misfire', choices=('skip', 'latest'), help='cron: skip expired minutes (default), or run the latest missed occurrence')
    c.add_argument('--start-window', help='cron script: local HH:MM-HH:MM, inclusive start, exclusive end; requires misfire skip')
    c.add_argument('--script', help='script file to execute without a model (shell/python or executable)')
    c.add_argument('--script-timeout', type=float, help='script timeout in seconds (default 300)')
    c.add_argument('--deliver', choices=('telegram', 'owner-inbox', 'local'), help='result destination (default owner-inbox)')
    cadence.add_argument('--every', type=parse_duration,
                   help='positive Ns, Nm, Nh, or Nd interval: a recurring card creating one instance per period')
    c.add_argument('--max-runs', type=int, help='with --every/--cron: maximum number of instances')
    c.add_argument('--until', type=parse_timestamp, help='with --every/--cron: offset-aware ISO-8601 time after which no instance starts')
    c.add_argument('--overlap', help='with --every/--cron: skip (default) a period while the previous instance runs, or queue it behind')
    c.add_argument('--wait', type=float, default=0, help='block up to N seconds for done/blocked')
    c.add_argument('--json', action='store_true')
    ls = sub.add_parser('list')
    ls.add_argument('--state', action='append', choices=store.STATES,
                    help='filter states; cancelled cards are hidden unless explicitly selected')
    ls.add_argument('--json', action='store_true')
    bd = sub.add_parser('board', help='the board: in progress, waiting with a badge, done within the window')
    bd.add_argument('--json', action='store_true')
    bd.add_argument('--archive', action='store_true', help='add the archived cards, newest first')
    bd.add_argument('--limit', type=int, default=50, help='with --archive: at most N archived cards (default 50)')
    sh = sub.add_parser('show')
    sh.add_argument('card')
    sh.add_argument('--json', action='store_true')
    sh.add_argument('--prompts', action='store_true')
    rl = sub.add_parser('release', help='lift the not_before hold of a created card (its trigger happened)')
    rl.add_argument('card')
    rl.add_argument('--by', default='secretary')
    ca = sub.add_parser('cancel', help='cancel a card and wait for its active heads to exit')
    ca.add_argument('card')
    ca.add_argument('--reason', default='', help='required when closing a failed blocked card')
    ca.add_argument('--if-version', type=int, help='refuse if card version changed')
    ca.add_argument('--keep-card', help='steward duplicate keeper id')
    ca.add_argument('--keep-version', type=int, help='steward keeper snapshot version')
    ca.add_argument('--steward', action='store_true', help='validate housekeeping reason inside cancellation transaction')
    ca.add_argument('--by', default='secretary', help='config owner_actor, secretary or steward')
    sp = sub.add_parser('stop', help='stop a recurring card: no new instances, existing ones untouched')
    sp.add_argument('card')
    sp.add_argument('--by', default='secretary')
    asg = sub.add_parser('assign', help='set or remove the worker executor of a card')
    asg.add_argument('card')
    who = asg.add_mutually_exclusive_group(required=True)
    who.add_argument('executor', nargs='?')
    who.add_argument('--none', action='store_true', help='remove the assignment')
    asg.add_argument('--on-unavailable', default='wait')
    asg.add_argument('--by', default='secretary')
    for name, text in (('depend', 'CARD waits in blocked until BLOCKER is done'),
                       ('undepend', 'drop the dependency of CARD on BLOCKER')):
        dp = sub.add_parser(name, help=text)
        dp.add_argument('card')
        dp.add_argument('blocker')
        dp.add_argument('--by', default='secretary')
    at = sub.add_parser('attach', help='make CARD a child of EPIC')
    at.add_argument('card')
    at.add_argument('epic')
    at.add_argument('--optional', action='store_true')
    at.add_argument('--by', default='secretary')
    dt_ = sub.add_parser('detach', help='remove CARD from EPIC')
    dt_.add_argument('card')
    dt_.add_argument('epic')
    dt_.add_argument('--by', default='secretary')
    st = sub.add_parser('steer', help='deliver a clarification to the live worker; restart only if input is unavailable')
    st.add_argument('card')
    st.add_argument('text')
    st.add_argument('--by', default='secretary')
    sub.add_parser('tick')
    d = sub.add_parser('daemon')
    d.add_argument('--interval', type=float, default=10)
    sub.add_parser('observe')
    sub.add_parser('ladder')
    a = ap.parse_args(argv)
    if a.cmd == 'create' and not (a.script or a.acceptance or a.accept_review):
        ap.error('one of the arguments --acceptance --accept-review is required (unless --script is set)')
    if a.cmd == 'board' and a.limit < 1:
        ap.error('argument --limit: must be a positive number of cards')

    cfg = store.load_config(a.config)
    tz = cfg['owner_tz']
    cancellers = (cfg['owner_actor'], 'secretary', 'steward')
    if a.cmd == 'cancel' and a.by not in cancellers:
        ap.error(f"argument --by: invalid choice: {a.by!r} (choose from {', '.join(map(repr, cancellers))})")
    # A CLI write waits busy_timeout (30 s) per attempt; ride out a stuck writer before failing.
    conn = store.connect(cfg['db'], begin_retries=0 if a.cmd in ('daemon', 'tick') else 2,
                         owner_actor=cfg['owner_actor'])

    if a.cmd == 'cancel':
        try:
            changed = dispatcher.Dispatcher(cfg, conn).cancel(a.card, a.reason, a.by, expected_version=a.if_version, steward_check=a.steward, keep_card=a.keep_card, keep_version=a.keep_version)
        except api.CardError as e:
            print(f'hive: {e}', file=sys.stderr)
            return 2
        print(f'{a.card} cancelled' + ('' if changed else ' (already cancelled)'))
        return 0

    if a.cmd == 'create':
        body = Path(a.body_file).read_text() if a.body_file else a.body
        judge = a.accept_review is not None
        try:
            cid, created = api.create(conn, title=a.title, goal=a.goal,
                                      acceptance='test -s REPORT.md' if judge else a.acceptance, step=a.step,
                                      source=a.source, created_by=a.created_by, card_class=a.card_class,
                                      owner_visible=a.owner_visible, followup_of=a.followup_of, review=a.review or judge, body=body,
                                      task_type=a.task_type, idem_key=a.idem_key, priority=a.priority,
                                      not_before=time.time() + 10 * 365 * 86400 if a.hold else a.not_before,
                                      acceptance_kind='judge' if judge else 'command',
                                      judge_criteria=a.accept_review if judge else None, deny_tools=a.deny_tools,
                                      recheck_every=a.recheck_every, recheck_until=a.recheck_until,
                                      recheck_max=a.recheck_max, blocked_by=a.blocked_by,
                                      kind='epic' if a.epic else 'recurring' if a.every is not None or a.cron is not None else 'task',
                                      parent=a.parent, optional=a.optional,
                                      assignee=a.assignee, on_unavailable=a.on_unavailable, executors=heads.executors(cfg),
                                      model=a.model, effort=a.effort, cfg=cfg, panel=a.panel, executor=a.executor,
                                      every=a.every, max_runs=a.max_runs, until=a.until, overlap=a.overlap,
                                      cron=a.cron, tz=a.tz, misfire=a.misfire, start_window=a.start_window, script=a.script,
                                      script_timeout=a.script_timeout, deliver=a.deliver, gate_timeout=a.gate_timeout)
        except api.CardError as e:
            print(f'hive: card rejected: {e}', file=sys.stderr)
            return 2
        card = api.wait(conn, cid, a.wait) if a.wait else api.get(conn, cid)
        if a.json:
            print(json.dumps({'id': cid, 'created': created, 'state': card['state'], 'card_class': card['card_class'], 'owner_visible': bool(card['owner_visible'])}, ensure_ascii=False))
        else:
            print(f"{cid} {'created' if created else 'exists (idempotency key)'} state={card['state']}")
        return 0

    if a.cmd == 'list':
        rows = [dict(r, admission_class=api.admission_class(r, cfg)) for r in api.listing(conn, a.state)]
        if a.json:
            print(json.dumps(rows, ensure_ascii=False))
        else:
            for r in rows:
                print(f"{r['id']}  {r['state']:<8} {r['admission_class']:<10} s{r['step']} r{r['round']}{' +review' if r['review'] else ''}"
                      f"{' @' + r['assignee'] if r['assignee'] else ''}{' executor=' + r['executor'] if r['executor'] else ''}  "
                      + (f"{r['model']}/{r['effort']}  " if r['model'] is not None else '') +
                      ('panel=' + ','.join(f"{m['label']}:{m['model']}/{m['effort']}" for m in r['panel']) + '  ' if r['panel'] else '') +
                      f"{ts(r['updated_at'], tz)}  {r['created_by']:<12} {'[' + r['kind'] + '] ' if r['kind'] != 'task' else ''}{r['title']}"
                      + (f"  (epic {r['parent']})" if r['parent'] else '')
                      + (f"  (series {r['series']} #{r['series_period']})" if r['series'] else ''))
        return 0

    if a.cmd == 'board':
        b = api.board(conn, time.time(), archive_limit=a.limit if a.archive else 0, alive=liveness.alive,
                      cgroup_root=liveness.CGROUP_ROOT)
        if a.json:
            print(json.dumps(b, ensure_ascii=False))
            return 0
        titles = {'in_progress': 'In progress', 'waiting': 'Waiting', 'done': 'Done', 'archive': 'Archive'}
        for name, entries in [*b['columns'].items(), *([('archive', b['archive'])] if a.archive else [])]:
            print(f"{titles[name]} ({len(entries)})")
            for e in entries:
                if name == 'waiting':
                    detail = f"[{e['badge']}] {e['cause']}" + (' ' + ','.join(e['cards']) if e['cards'] else '') \
                        + (f" until {ts(e['until'], tz)}" if e['until'] is not None else '') \
                        + (f": {e['reason']}" if e['reason'] else '')
                elif e['finished_at'] is not None:
                    detail = f"{e['state']} {ts(e['finished_at'], tz)}"
                else:
                    detail = e['state']
                print(f"  {e['id']}  {detail}  {e['title']}")
        return 0

    if a.cmd == 'show':
        card = api.get(conn, a.card)
        if card is None:
            print('no such card', file=sys.stderr)
            return 1
        if a.json:
            print(json.dumps(card, ensure_ascii=False, default=str, indent=1))
            return 0
        print(f"{card['id']} [{card['state']}] {card['title']}\n  goal: {card['goal']}\n  acceptance: {card['acceptance']}")
        print(f"  step {card['step']} · review {'on' if card['review'] else 'off'} · round {card['round']} · by {card['created_by']} · source {card['source']}")
        print(f"  workdir: {card['workdir'] or '—'}  result: {card['result'] or '—'}")
        print(f"  assignee: {card['assignee'] or '—'} · on unavailable: {card['on_unavailable']} · not before: {ts(card['not_before'], tz)}")
        if card['executor'] is not None:
            print(f"  executor: {card['executor']}")
        if card['model'] is not None:
            print(f"  model: {card['model']} · effort: {card['effort']}")
        if card['panel']:
            print('  panel: ' + ', '.join(f"{m['label']} {m['vendor']}:{m['model']}/{m['effort']}" for m in card['panel']))
        if card['executor_wait']:
            print(f"  waiting for executor {card['executor_wait']['executor']}: {card['executor_wait']['reason']}")
        if card['parent']:
            p = card['parent']
            print(f"  epic: {p['id']} [{p['state']}]{' optional' if p['optional'] else ''}")
        if card['progress'] is not None:
            pr = card['progress']
            print(f"  progress: {pr['done']}/{pr['total']} required done; blocked: {pr['blocked_waiting']} waiting, "
                  f"{pr['blocked_failed']} failed")
            for ch in card['children']:
                print(f"  child {ch['id']} [{ch['state']}]{' optional' if ch['optional'] else ''}")
        if card['schedule'] is not None:
            sc = card['schedule']
            cadence_text = f"cron {sc['cron']} ({sc['tz']})" if sc.get('cron') else f"every {sc['every']:g}s"
            print(f"  {cadence_text} · max runs {sc['max_runs'] or '—'} · until {ts(sc['until'], tz)} · overlap {sc['overlap']}"
                  f" · start {ts(sc['start'], tz)}{' · stopped ' + ts(sc['stopped_at'], tz) if sc['stopped'] else ''}")
            print(f"  next start: {ts(sc['next_start'], tz)} · created {sc['created']} · skipped {sc['skipped']}")
            if sc.get('cron'):
                print(f"  misfire: {sc['misfire']} · start window: {sc['start_window'] or 'unrestricted'}")
            for inst in card['instances']:
                print(f"  instance #{inst['period']} {inst['id']} [{inst['state']}] "
                      f"{inst['block_reason'] or inst['wait_reason'] or inst['result'] or ''}".rstrip())
        if card['series']:
            print(f"  series: {card['series']['id']} period #{card['series']['period']}")
        if card['series_start_window'] is not None:
            print(f"  start window: {card['series_start_window']} ({card['series_tz']}) · completion: existing script timeout")
            if card['kind'] != 'recurring':
                print(f"  eligible: [{ts(card['eligible_from'], tz)}, {ts(card['eligible_until'], tz)})")
        if card['blocked_by']:
            print('  blocked by: ' + ', '.join(f"{b['id']} [{b['state']}]" for b in card['blocked_by']))
        for reason in card['block_reasons']:
            print(f"  block reason: {json.dumps(reason, ensure_ascii=False)}")
        for r in card['runs']:
            print(f"  run {r['id']} r{r['round']} {r['member'] or r['role']:<8} {r['vendor']}:{r['model']}/{r['effort']} {r['status']} "
                  f"exit={r['exit_code']} {r['duration_s']}s tok={r['tokens_in']}/{r['tokens_out']} {r['verdict'] or ''}"
                  + (f" job={r['job']}" if r['job'] else ''))
            if a.prompts:
                print('    prompt:\n      ' + r['prompt'].replace('\n', '\n      '))
            if r['report']:
                print('    report: ' + r['report'][:600].replace('\n', '\n      '))
        for g in card['gates']:
            print(f"  gate r{g['round']} exit={g['exit_code']} {'PASS' if g['passed'] else 'FAIL'} {(g['output'] or '').strip()[:200]}")
        for st in card['steers']:
            print(f"  steer {st['id']} by {st['by']}: {st['delivery']}" + (f" (run {st['run']})" if st['run'] else '')
                  + (f" via {st['method']}" if st['method'] else '') + (f"; {st['reason']}" if st['reason'] else ''))
        for e in card['events']:
            print(f"  {ts(e['at'], tz)} {e['kind']} {json.dumps(e['data'], ensure_ascii=False) if e['data'] else ''}"[:300])
        return 0

    if a.cmd == 'release':
        try:
            ok = api.release(conn, a.card, actor=a.by)
        except api.CardError as e:
            print(f'hive: release rejected: {e}', file=sys.stderr)
            return 2
        print(f"{a.card} {'released' if ok else 'not released (missing, or neither created nor waiting)'}")
        return 0 if ok else 1

    if a.cmd == 'stop':
        try:
            changed = api.stop(conn, a.card, actor=a.by)
        except api.CardError as e:
            print(f'hive: stop rejected: {e}', file=sys.stderr)
            return 2
        print(f"{a.card} {'stopped' if changed else 'unchanged (already stopped or final)'}")
        return 0

    if a.cmd == 'assign':
        try:
            changed = api.assign(conn, a.card, None if a.none else a.executor, on_unavailable=a.on_unavailable,
                                 executors=heads.executors(cfg), actor=a.by)
        except api.CardError as e:
            print(f'hive: assignment rejected: {e}', file=sys.stderr)
            return 2
        card = api.get(conn, a.card)
        print(f"{a.card} assign: {'changed' if changed else 'unchanged'} assignee={card['assignee'] or '—'} "
              f"on_unavailable={card['on_unavailable']}")
        return 0

    if a.cmd in ('depend', 'undepend'):
        change = api.add_blocker if a.cmd == 'depend' else api.remove_blocker
        try:
            changed = change(conn, a.card, a.blocker, actor=a.by)
        except api.CardError as e:
            print(f'hive: dependency rejected: {e}', file=sys.stderr)
            return 2
        state = api.get(conn, a.card)['state']
        print(f"{a.card} {a.cmd} {a.blocker}: {'changed' if changed else 'unchanged'} state={state}")
        return 0

    if a.cmd in ('attach', 'detach'):
        try:
            if a.cmd == 'attach':
                changed = api.add_child(conn, a.card, a.epic, optional=a.optional, actor=a.by)
            else:
                changed = api.remove_child(conn, a.card, a.epic, actor=a.by)
        except api.CardError as e:
            print(f'hive: epic link rejected: {e}', file=sys.stderr)
            return 2
        print(f"{a.card} {a.cmd} {a.epic}: {'changed' if changed else 'unchanged'}")
        return 0

    if a.cmd == 'steer':
        try:
            sid = api.steer(conn, a.card, a.text, actor=a.by)
        except api.CardError as e:
            print(f'hive: steer rejected: {e}', file=sys.stderr)
            return 2
        st = next(s for s in api.steer_deliveries(conn, a.card) if s['id'] == sid)
        card = conn.execute('SELECT state, round, wait_reason FROM cards WHERE id=?', (a.card,)).fetchone()
        then = 'a new worker head gets it in the same workspace'
        closed = store.is_final(card)
        why = {'pending': f'not delivered yet: {then}',
               'pending-live': f"not delivered yet: worker run {st['run']} has not seen it; live delivery pending, restart only if input is unavailable",
               'delivered-live': f"delivered-live to worker run {st['run']}; waiting for its report acknowledgement",
               'restarted': f"restarted: handed off to worker run {st['run']}; {st['reason']}",
               'stopping': f"not delivered yet: worker run {st['run']} has not seen it; the dispatcher stops that run on its next tick and {then}",
               'handed_off': f"handed off to worker run {st['run']} in its prompt, not acknowledged until its report quotes it",
               'acknowledged': f"acknowledged: the report of worker run {st['run']} quotes it",
               'unacknowledged': f"not acknowledged: worker run {st['run']} ended without quoting it in its report; "
                                 + (f"the card is {card['state']}" if closed else 'the next worker round must quote it or the card blocks'),
               'undelivered': 'not delivered: the card is closed'}[st['delivery']]
        print(f"{a.card} steer {sid} recorded, {why}. `hive.py show {a.card}` prints its delivery")
        return 0

    if a.cmd == 'tick':
        print(json.dumps(dispatcher.Dispatcher(cfg, conn).tick(), ensure_ascii=False))
        return 0

    if a.cmd == 'daemon':
        while True:
            try:
                cfg = store.load_config(a.config)   # ladder/limits edits apply without restart
                d = dispatcher.Dispatcher(cfg, conn)
                s = d.tick()
                if s.get('launched') or s.get('advanced') or s.get('published') or s.get('reaped'):
                    print(json.dumps(s, ensure_ascii=False), flush=True)
            except Exception as e:  # keep the loop alive; the error is visible in the journal
                print(f'tick error: {type(e).__name__}: {e}', file=sys.stderr, flush=True)
            time.sleep(a.interval)

    if a.cmd == 'observe':
        cards = {s: conn.execute('SELECT count(*) FROM cards WHERE state=?', (s,)).fetchone()[0] for s in store.STATES}
        print(json.dumps({'counts': cards, 'anomalies': dispatcher.observe(conn, cfg)}, ensure_ascii=False, indent=1))
        return 0

    if a.cmd == 'ladder':
        for step in sorted(cfg['ladder'], reverse=True):
            row = cfg['ladder'][step]
            print(f"{step}: " + ' / '.join(f"{v} {h['model']} {h['effort']}" for v, h in row.items()))
        return 0


if __name__ == '__main__':
    sys.exit(main())
