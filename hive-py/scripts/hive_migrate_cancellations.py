#!/usr/bin/env python3
"""Audited legacy cancellation migration. Dry-run unless --apply is supplied."""
import argparse
import json
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from hive import api, dispatcher, store


def candidates(conn, owner, extra=()):
    """{card: (reason, actor)} of legacy cancellations; `owner` is config owner_actor."""
    found = {}
    for row in conn.execute("SELECT * FROM cards WHERE state='blocked'"):
        try:
            reason = json.loads(row['block_reason'] or '{}')
        except (ValueError, TypeError):
            continue
        if isinstance(reason, dict) and reason.get('kind') == 'cancelled':
            found[row['id']] = (reason.get('note') or reason.get('reason') or 'legacy cancellation',
                                reason.get('by') if reason.get('by') in (owner, 'secretary') else 'secretary')
    for cid in extra:
        row = api.card_row(conn, cid)
        if row['state'] == 'cancelled':
            continue
        if store.is_final(row) and cid not in found:
            raise api.CardError(f'{cid} is final without a legacy cancellation marker')
        found.setdefault(cid, ('Owner cancellation: audited legacy card', owner))
    return found


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument('--config')
    ap.add_argument('--card', action='append', default=[])
    ap.add_argument('--apply', action='store_true')
    a = ap.parse_args()
    cfg = store.load_config(a.config)
    conn = store.connect(cfg['db'], owner_actor=cfg['owner_actor'])
    pending = candidates(conn, cfg['owner_actor'], a.card)
    d = dispatcher.Dispatcher(cfg, conn)
    for cid, (reason, actor) in pending.items():
        if a.apply:
            d.cancel(cid, reason, actor, migrate=True)
        print(json.dumps(dict(card=cid, reason=reason, by=actor, applied=a.apply), ensure_ascii=False))


if __name__ == '__main__':
    main()
