#!/usr/bin/env python3
"""Migrate Hive classes and export ambiguous historical assignments as JSON."""
import argparse
import json
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from hive import store


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--db', required=True)
    parser.add_argument('--report', required=True)
    parser.add_argument('--config', help='hive config naming owner_actor (default: HIVE_CONFIG)')
    args = parser.parse_args()
    conn = store.connect(args.db, owner_actor=store.load_config(args.config)['owner_actor'])
    rows = [dict(card_id=r['card_id'], **json.loads(r['data'])) for r in conn.execute("SELECT card_id,data FROM events WHERE kind='class_migrated' ORDER BY id")]
    report = dict(counts={r['card_class']: r['n'] for r in conn.execute('SELECT card_class,COUNT(*) n FROM cards GROUP BY card_class')},
                  ambiguous=[r for r in rows if r['ambiguous']], owner_visible_policy='Historical series results default to false; no report text inference.')
    Path(args.report).write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps(dict(counts=report['counts'], ambiguous=len(report['ambiguous']))))


if __name__ == '__main__':
    main()
