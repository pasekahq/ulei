"""Detached gate worker. No dispatcher lock is acquired while running shell acceptance or the judge's model turn."""
import sys
from . import store
from .dispatcher import Dispatcher


def main():
    config, db, card_id, version, mode = sys.argv[1:]
    cfg = store.load_config(config)
    cfg['db'] = db
    conn = store.connect(db, owner_actor=cfg['owner_actor'])
    try:
        card = None
        try:
            card = conn.execute('SELECT * FROM cards WHERE id=? AND version=?', (card_id, int(version))).fetchone()
            if card is not None:
                d = Dispatcher(cfg, conn)
                getattr(d, mode)(card)
        finally:
            release(conn, card_id, int(version), mode)
        if card is None:
            return
        card = conn.execute('SELECT * FROM cards WHERE id=?', (card_id,)).fetchone()
        # A passed gate hands over to the judge in this process, so the result does not wait for a tick.
        if mode == 'gate' and d.judge_due(card) and d.claim(card, 'judge'):
            try:
                d.judge(card)
            finally:
                release(conn, card_id, card['version'], 'judge')
        publish_card(d, card_id)
    finally:
        conn.close()


def release(conn, card_id, version, mode):
    conn.execute('DELETE FROM gate_jobs WHERE card_id=? AND version=? AND mode=?', (card_id, version, mode))


def publish_card(d, card_id):
    """Deliver this card's result now instead of on the next dispatcher tick.
    Publish is idempotent per outbox row; a failure stays unpublished for the tick to retry."""
    try:
        for (event_id,) in d.conn.execute('SELECT event_id FROM outbox WHERE card_id=? AND published_at IS NULL',
                                          (card_id,)).fetchall():
            d.publish(event_id)
    except Exception as e:  # never fail the gate over delivery; the tick retries
        print(f'hive.gate: publish {card_id}: {type(e).__name__}: {e}', file=sys.stderr)


if __name__ == '__main__':
    main()
