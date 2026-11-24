#!/usr/bin/env python3
"""Daily model steward. Default is a model dry-run; --digest reads cached owner questions."""
import argparse
from datetime import datetime
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import time
from zoneinfo import ZoneInfo

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
from hive import heads, inbox_library, steward, store


def snapshot(cfg):
    conn = sqlite3.connect(Path(cfg['db']).resolve().as_uri() + '?mode=ro', uri=True)
    conn.row_factory = sqlite3.Row
    try:
        conn.execute('BEGIN')
        return steward.board(conn, cfg)
    finally:
        conn.close()


def record(path, item):
    with path.open('a', encoding='utf-8') as f:
        f.write(json.dumps(item, ensure_ascii=False) + '\n')
        f.flush()
        os.fsync(f.fileno())


def save(path, value):
    temp = path.with_suffix(path.suffix + '.tmp')
    temp.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n')
    temp.replace(path)


def alert(cfg, stamp, error):
    """Clear technical alert for the secretary instead of a traceback; returns True once published.
    The text is config `texts.steward_alert` with {date} and {error} filled in, then the error as JSON."""
    queue = inbox_library.open_inbox(cfg)
    raw = steward.redact(getattr(error, 'raw', ''), 1500)
    digest = hashlib.sha256(f'{stamp}:{type(error).__name__}:{error}:{raw}'.encode()).hexdigest()[:24]
    event_id = f'hive-steward-error:{stamp}:{digest}'
    if queue.get(event_id) is None:
        text = (cfg['texts']['steward_alert'].format(date=stamp, error=f'{type(error).__name__}: {error}')
                + json.dumps({'source': 'hive-steward', 'date': stamp, 'error': str(error), 'raw_excerpt': raw},
                             ensure_ascii=False))
        queue.publish(queue.packet('task_result', event_id, text, time.time()))
    return queue.get(event_id) is not None


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__)
    mode = p.add_mutually_exclusive_group()
    mode.add_argument('--apply', action='store_true')
    mode.add_argument('--digest', action='store_true')
    p.add_argument('--config')
    p.add_argument('--vendor', choices=heads.VENDORS, default=heads.VENDORS[0],
                   help='provenance label kept in the daily plan; the model is the [steward] route')
    p.add_argument('--output', type=Path, help='steward directory (default: config steward_dir)')
    a = p.parse_args(argv)
    cfg = store.load_config(a.config)
    a.output = a.output or Path(cfg['steward_dir'])
    tz = ZoneInfo(cfg['owner_tz'])
    if a.digest:
        # Briefing must never make a model call or invent age-based questions.
        stamp = datetime.now(tz).date().isoformat()
        cached = a.output / f'{stamp}.json'
        result = json.loads(cached.read_text()) if cached.exists() else {'propose': []}
        print((steward.digest(result) if result.get('model_review') else '') or '[SILENT]')
        return 0
    if not a.apply:
        context = snapshot(cfg)
        result = steward.model_plan(cfg, context, steward.query_model(cfg, context))
        print(json.dumps(dict(result, vendor=a.vendor), ensure_ascii=False, indent=2))
        return 0
    # Persistent timers may catch up at night. Do no writes outside morning.
    local = datetime.now(tz)
    if not 7 <= local.hour < 12:
        print(f"steward: outside 07:00–12:00 {cfg['owner_tz']}; skipped")
        return 0
    a.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (a.output / 'steward.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        stamp = local.date().isoformat()
        completed = a.output / 'completed-date'
        if completed.exists() and completed.read_text().strip() == stamp:
            return 0
        daily = a.output / f'{stamp}.json'
        result = json.loads(daily.read_text()) if daily.exists() else {}
        if result.get('model_review') and result.get('vendor') != a.vendor:
            print(f"steward: reviewed daily plan vendor provenance mismatch: "
                  f"cached {result.get('vendor')!r}, selected {a.vendor!r}; cached plan preserved",
                  file=sys.stderr)
            return 1
        if not result.get('model_review'):
            context = snapshot(cfg)
            try:
                result = steward.model_plan(cfg, context, steward.query_model(cfg, context))
            except ValueError as e:
                # Failed turn or unusable answer: nothing was cancelled; report it, keep the day open for a retry.
                print(f'steward: no usable model plan: {type(e).__name__}: {e}', file=sys.stderr)
                save(a.output / 'last-error.json', {'date': stamp, 'at': time.time(), 'error': str(e),
                                                    'raw_excerpt': steward.redact(getattr(e, 'raw', ''), 1500)})
                try:
                    published = alert(cfg, stamp, e)
                except Exception as exc:  # noqa: BLE001 - the unit must stay failed if the alert is lost
                    print(f'steward: owner-inbox alert failed: {type(exc).__name__}: {exc}', file=sys.stderr)
                    published = False
                print(json.dumps({'model_error': str(e), 'alerted': published}, ensure_ascii=False))
                return 0 if published else 1
            result.update(model_review=True, vendor=a.vendor)
            save(daily, result)  # Reuse the validated plan on retry; no second daily model call.
        journal = a.output / 'journal.jsonl'
        errors = []
        for item in result['cancel']:
            # Journal intent before calling cancel, so a crash always leaves a trail.
            record(journal, dict(item, at=time.time(), phase='intent'))
            command = [sys.executable, str(ROOT / 'scripts/hive.py')]
            if a.config:
                command += ['--config', a.config]
            command += ['cancel', item['id'], '--reason', item['reason'], '--by', 'steward',
                        '--if-version', str(item['version'])]
            if item.get('keep_id'):
                command += ['--keep-card', item['keep_id'], '--keep-version', str(item['keep_version'])]
            try:
                r = subprocess.run(command, capture_output=True, text=True, timeout=180)
                outcome = dict(item, at=time.time(), phase='result', exit_code=r.returncode,
                               output=(r.stdout + r.stderr)[-2000:])
            except (OSError, subprocess.TimeoutExpired) as e:
                outcome = dict(item, at=time.time(), phase='result', exit_code=-1, output=str(e))
            record(journal, outcome)
            if outcome['exit_code']:
                errors.append(outcome)
        result['results_failed'] = errors
        save(daily, result)
        (a.output / 'latest.md').write_text(steward.digest(result) + '\n')
        if not errors:
            completed.write_text(stamp + '\n')
        summary = {'cancel_candidates': len(result['cancel']), 'failed': len(errors),
                   'proposed': len(result['propose'])}
        if 'selection' in result:
            summary['selection'] = result['selection']
        print(json.dumps(summary))
        return int(bool(errors))


if __name__ == '__main__':
    raise SystemExit(main())
