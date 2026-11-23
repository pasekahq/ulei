"""Persisted card provenance. Source inference is only a legacy creation fallback."""
CLASSES = ('owner_task', 'series_run', 'system', 'followup')


def legacy_class(source, created_by='', series=False):
    if series:
        return 'series_run'
    prefix = (source or '').split(':', 1)[0]
    if prefix in ('msg', 'ledger', 'issue'):
        return 'owner_task'
    if prefix == 'cron':
        return 'series_run'
    if prefix in ('system', 'watch', 'steward', 'auto', 'event'):
        return 'system'
    return None


def migrate(conn, owner_actor):
    """One transactional migration; preserve ambiguous decisions as durable events.
    owner_actor is the owner's created_by; it is required when the store holds cards to classify."""
    columns = {r['name'] for r in conn.execute('PRAGMA table_info(cards)')}
    if 'card_class' in columns:
        return
    conn.execute('BEGIN IMMEDIATE')
    try:
        # Recheck under the lock when two processes open the old database together.
        if 'card_class' in {r['name'] for r in conn.execute('PRAGMA table_info(cards)')}:
            conn.execute('COMMIT')
            return
        conn.execute("ALTER TABLE cards ADD COLUMN card_class TEXT NOT NULL DEFAULT 'system' CHECK(card_class IN ('owner_task','series_run','system','followup'))")
        conn.execute('ALTER TABLE cards ADD COLUMN owner_visible INTEGER NOT NULL DEFAULT 0 CHECK(owner_visible IN (0,1))')
        import json, time
        rows = conn.execute('SELECT * FROM cards').fetchall()
        if rows and owner_actor is None:
            raise ValueError('config: missing key owner_actor, required to classify cards without card_class')
        for row in rows:
            series = row['kind'] == 'recurring' or row['series_period'] is not None or conn.execute("SELECT 1 FROM links WHERE card_id=? AND kind IN ('series','cron-agent')", (row['id'],)).fetchone()
            inferred = legacy_class(row['source'], row['created_by'], series)
            creator = row['created_by'] or ''
            fallback = 'owner_task' if creator == owner_actor else 'series_run' if creator.startswith('cron:') else 'system'
            chosen = inferred or fallback
            ambiguous = inferred is None or (creator == owner_actor and chosen != 'owner_task' and not series) or (creator.startswith('cron:') and chosen != 'series_run')
            conn.execute('UPDATE cards SET card_class=?, owner_visible=? WHERE id=?', (chosen, int(chosen == 'owner_task'), row['id']))
            conn.execute("INSERT INTO events(card_id,at,kind,actor,data) VALUES(?,?,'class_migrated','migration',?)", (row['id'], time.time(), json.dumps(dict(card_class=chosen, ambiguous=ambiguous, source=row['source'], created_by=creator))))
        conn.execute('COMMIT')
    except BaseException:
        conn.execute('ROLLBACK')
        raise
