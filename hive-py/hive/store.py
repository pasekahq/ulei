"""SQLite store: schema, connections, transactions, config.

Every card transition is a compare-and-swap (state + version) inside a
BEGIN IMMEDIATE transaction. Idempotency keys are UNIQUE in the schema, so a
race between two creators is decided by the database, not by a pre-check.
"""
import contextlib
import ctypes
import json
import os
import re
import sqlite3
import sys
import threading
import time
import tomllib
from pathlib import Path
from zoneinfo import ZoneInfo

BUSY_TIMEOUT_S = 30

STATES = ('created', 'queued', 'running', 'check', 'review', 'done', 'blocked', 'cancelled')
TERMINAL = ('done', 'blocked', 'cancelled')
ACTIVE_RUN = ('starting', 'running')
FINISHED_RUN = ('exited', 'timeout', 'crashed', 'failed_launch', 'interrupted')
SYNTHESIS = '@synthesis'
EXECUTOR_LIMITS = ('poll_s', 'call_timeout_s', 'timeout_s', 'cooldown_s')
EXECUTOR_COUNTS = ('max_parallel', 'daily_sends')
ADMISSION_DEFAULTS = {'urgent_max_step': 2, 'urgent_reserve': 1, 'urgent_floor_mb': 768}
# Deployment values without a default: a config that lacks one is refused with the key's name.
REQUIRED_KEYS = ('owner_actor', 'owner_tz', 'inbox_library', 'results_dir', 'card_show_command', 'head_env',
                 'gate_env', 'script_env', 'script_env_file', 'steward_dir', 'rotation_log', 'texts', 'gateway_socket',
                 'judge', 'steward')
JUDGE_NUMBERS = ('max_tokens', 'timeout_s', 'retry_s')
STEWARD_NUMBERS = ('max_tokens', 'timeout_s')
ROUTE_ALIAS = re.compile(r'[a-z][a-z0-9_-]{0,63}')
TEXTS = ('workspace_context', 'report_request', 'series_news_note', 'result_note', 'missed_note',
         'steward_prompt', 'steward_alert')
ENV_TABLES = ('head_env', 'gate_env', 'script_env')

RUNS_TABLE = """CREATE TABLE IF NOT EXISTS runs(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  card_id TEXT NOT NULL REFERENCES cards(id),
  round INTEGER NOT NULL, role TEXT NOT NULL CHECK(role IN ('worker','reviewer')),
  vendor TEXT NOT NULL, model TEXT NOT NULL, effort TEXT NOT NULL,
  step INTEGER NOT NULL, task_type TEXT, prompt TEXT NOT NULL,
  status TEXT NOT NULL, handled INTEGER NOT NULL DEFAULT 0,
  pid INTEGER, unit TEXT, run_dir TEXT,
  started_at REAL NOT NULL, heartbeat_at REAL, ended_at REAL, duration_s REAL,
  exit_code INTEGER, outcome TEXT, verdict TEXT,
  tokens_in INTEGER, tokens_out INTEGER, tokens_json TEXT,
  report TEXT, report_sha256 TEXT, report_tail TEXT, error TEXT, provider_failure TEXT, member TEXT, report_file TEXT, job TEXT);
"""

SCHEMA = """
CREATE TABLE IF NOT EXISTS cards(
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL DEFAULT 'task',
  lane TEXT NOT NULL DEFAULT 'task',
  title TEXT NOT NULL, goal TEXT NOT NULL, acceptance TEXT NOT NULL,
  acceptance_kind TEXT NOT NULL DEFAULT 'command' CHECK(acceptance_kind IN ('command', 'judge')),
  judge_criteria TEXT,
  deny_tools TEXT NOT NULL DEFAULT '[]',
  body TEXT NOT NULL DEFAULT '',
  step INTEGER NOT NULL CHECK(step BETWEEN 1 AND 5),
  review INTEGER NOT NULL DEFAULT 0,
  task_type TEXT NOT NULL DEFAULT 'general',
  source TEXT NOT NULL, created_by TEXT NOT NULL,
  idem_key TEXT UNIQUE,
  priority INTEGER NOT NULL DEFAULT 0,
  not_before REAL,
  recheck_every REAL,
  recheck_until REAL,
  recheck_max INTEGER,
  recheck_due REAL,
  recheck_attempts INTEGER NOT NULL DEFAULT 0,
  assignee TEXT, executor TEXT,
  model TEXT, effort TEXT, panel TEXT,
  on_unavailable TEXT NOT NULL DEFAULT 'wait',
  executor_wait TEXT,
  series_every REAL, series_max INTEGER, series_until REAL, series_overlap TEXT, series_stopped REAL,
  series_next INTEGER, series_skipped INTEGER, series_period INTEGER,
  state TEXT NOT NULL, round INTEGER NOT NULL DEFAULT 0,
  version INTEGER NOT NULL DEFAULT 0,
  workdir TEXT, block_reason TEXT, wait_reason TEXT, result TEXT, feedback TEXT,
  created_at REAL NOT NULL, updated_at REAL NOT NULL, finished_at REAL);
CREATE INDEX IF NOT EXISTS cards_state ON cards(lane, state, priority, created_at);
CREATE TABLE IF NOT EXISTS gates(
  id INTEGER PRIMARY KEY AUTOINCREMENT, card_id TEXT NOT NULL, round INTEGER NOT NULL,
  command TEXT NOT NULL, exit_code INTEGER, passed INTEGER NOT NULL, output TEXT,
  at REAL NOT NULL, duration_s REAL, UNIQUE(card_id, round));
CREATE TABLE IF NOT EXISTS events(
  id INTEGER PRIMARY KEY AUTOINCREMENT, card_id TEXT, at REAL NOT NULL,
  kind TEXT NOT NULL, actor TEXT NOT NULL DEFAULT 'dispatcher', data TEXT);
CREATE INDEX IF NOT EXISTS events_card ON events(card_id, id);
CREATE TABLE IF NOT EXISTS outbox(
  event_id TEXT PRIMARY KEY, card_id TEXT NOT NULL, kind TEXT NOT NULL,
  payload TEXT NOT NULL, source_ts REAL NOT NULL, created_at REAL NOT NULL,
  published_at REAL, error TEXT);
CREATE TABLE IF NOT EXISTS links(
  card_id TEXT NOT NULL REFERENCES cards(id), kind TEXT NOT NULL, target_id TEXT NOT NULL REFERENCES cards(id),
  created_at REAL NOT NULL, optional INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(card_id, kind, target_id));
CREATE INDEX IF NOT EXISTS links_target ON links(target_id, kind);
CREATE UNIQUE INDEX IF NOT EXISTS links_one_parent ON links(card_id) WHERE kind='parent';
CREATE TABLE IF NOT EXISTS gate_jobs(
  card_id TEXT NOT NULL, version INTEGER NOT NULL, mode TEXT NOT NULL, started_at REAL NOT NULL,
  PRIMARY KEY(card_id, version, mode));
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT, at REAL);
"""

# Every dispatcher tick (10 s) reads these. Without them each tick scanned runs, cards, outbox and the
# overflow pages of every series instance (~100 MB of page reads, ~1 s CPU). Additive; DROP INDEX reverts.
TICK_INDEXES = """
CREATE INDEX IF NOT EXISTS cards_by_state ON cards(state, kind);
CREATE INDEX IF NOT EXISTS cards_recheck_due ON cards(state, recheck_due) WHERE recheck_due IS NOT NULL;
CREATE INDEX IF NOT EXISTS cards_series_member ON cards(id, series_period, state, wait_reason);
CREATE INDEX IF NOT EXISTS runs_by_card ON runs(card_id, role, round);
CREATE INDEX IF NOT EXISTS runs_by_status ON runs(status, vendor);
CREATE INDEX IF NOT EXISTS runs_unhandled ON runs(handled, status);
CREATE INDEX IF NOT EXISTS outbox_unpublished ON outbox(created_at) WHERE published_at IS NULL;
CREATE INDEX IF NOT EXISTS events_steer_stop ON events(id) WHERE kind='steer_stop';
"""

SERIES_COLUMNS = {'gate_timeout': 'REAL', 'series_every': 'REAL', 'series_max': 'INTEGER', 'series_until': 'REAL', 'series_overlap': 'TEXT',
                  'series_stopped': 'REAL', 'series_next': 'INTEGER', 'series_skipped': 'INTEGER',
                  'series_period': 'INTEGER', 'series_cron': 'TEXT', 'series_tz': 'TEXT', 'series_due': 'REAL',
                  'series_misfire': 'TEXT', 'series_start_window': 'TEXT',
                  'eligible_from': 'REAL', 'eligible_until': 'REAL', 'script': 'TEXT', 'script_timeout': 'REAL',
                  'deliver': "TEXT NOT NULL DEFAULT 'owner-inbox'",
                  'retry_at': 'REAL', 'retry_streak': 'INTEGER NOT NULL DEFAULT 0'}


def load_config(path=None):
    """The config at `path`, else at HIVE_CONFIG; texts named under [texts] are read relative to its directory."""
    path = path or os.environ.get('HIVE_CONFIG')
    if not path:
        raise ValueError('config: no path given and HIVE_CONFIG is not set')
    path = Path(path)
    with path.open('rb') as f:
        cfg = tomllib.load(f)
    if os.environ.get('HIVE_DB'):
        cfg['db'] = os.environ['HIVE_DB']
    cfg['ladder'] = {int(k): v for k, v in cfg['ladder'].items()}
    cfg['timeouts'] = {int(k): v for k, v in cfg.get('timeouts', {}).items()}
    for name, ec in cfg.get('executors', {}).items():
        check_executor(name, ec)
    if 'retry_backoff' in cfg:
        check_retry_backoff(cfg['retry_backoff'])
    cfg['admission'] = {**ADMISSION_DEFAULTS, **cfg.get('admission', {})}
    check_admission(cfg['admission'])
    check_deployment(cfg)
    cfg['texts'] = {name: (path.parent / cfg['texts'][name]).read_text(encoding='utf-8') for name in TEXTS}
    cfg['_path'] = str(path)
    return cfg


def missing(key):
    return ValueError(f'config: missing key {key}')


def check_deployment(cfg):
    """Refuse a config without a deployment value: the keys of REQUIRED_KEYS and TEXTS, the home of each vendor
    that runs heads, the disk and cgroups of enabled [resources] and the home of a configured limits command."""
    for key in REQUIRED_KEYS:
        if key not in cfg:
            raise missing(key)
    for key in ENV_TABLES:
        table = cfg[key]
        if not isinstance(table, dict) or not all(isinstance(v, str) for v in table.values()):
            raise ValueError(f'config: {key} must be a table of strings, got {table!r}')
    for name in TEXTS:
        if name not in cfg['texts']:
            raise missing(f'texts.{name}')
    ZoneInfo(cfg['owner_tz'])
    from .heads import VENDORS
    for name, vc in cfg.get('vendors', {}).items():
        if name in VENDORS and 'home' not in vc:
            raise missing(f'vendors.{name}.home')
    res = cfg.get('resources', {})
    if res.get('enabled', False):
        for key in ('disk', 'psi_cgroups'):
            if key not in res:
                raise missing(f'resources.{key}')
    if cfg.get('budget', {}).get('limits_command') and 'limits_home' not in cfg['budget']:
        raise missing('budget.limits_home')
    if not isinstance(cfg['gateway_socket'], str) or not cfg['gateway_socket']:
        raise ValueError(f"config: gateway_socket must be a path, got {cfg['gateway_socket']!r}")
    cfg['judge'] = check_judge(cfg['judge'])
    check_steward(cfg['steward'])


def check_numbers(table, name, keys):
    """Refuse `table` [name] without a positive integer max_tokens or a positive number for the other `keys`."""
    for key in keys:
        value = table.get(key)
        if value is None:
            raise missing(f'{name}.{key}')
        kinds = int if key == 'max_tokens' else (int, float)
        if isinstance(value, bool) or not isinstance(value, kinds) or value <= 0:
            raise ValueError(f'config: {name}.{key} must be a positive number, got {value!r}')


def check_route(alias, key):
    if not isinstance(alias, str) or not ROUTE_ALIAS.fullmatch(alias):
        raise ValueError(f'config: {key} must be a C4G route alias, got {alias!r}')


def check_judge(jc):
    """The [judge] table with `routes` keyed by step; refused without a positive integer max_tokens, positive
    timeout_s and retry_s, or a C4G route alias for every step 1..5."""
    check_numbers(jc, 'judge', JUDGE_NUMBERS)
    if 'routes' not in jc:
        raise missing('judge.routes')
    routes = {int(step): alias for step, alias in jc['routes'].items()}
    for step in range(1, 6):
        if step not in routes:
            raise missing(f'judge.routes.{step}')
        check_route(routes[step], f'judge.routes.{step}')
    return {**jc, 'routes': routes}


def check_steward(sc):
    """Refuse a [steward] table without a C4G route alias, a positive integer max_tokens or a positive timeout_s."""
    if 'route' not in sc:
        raise missing('steward.route')
    check_route(sc['route'], 'steward.route')
    check_numbers(sc, 'steward', STEWARD_NUMBERS)


def check_executor(name, ec):
    """Refuse an [executors.NAME] section without a client command, a positive time limit or a positive admission
    count, or with a refuse_task_types that is not a list of strings."""
    if not ec.get('command'):
        raise ValueError(f'executor {name}: command is required')
    for key in EXECUTOR_LIMITS + EXECUTOR_COUNTS:
        value = ec.get(key)
        if value is None:
            raise ValueError(f'executor {name}: {key} is required')
        kinds = int if key in EXECUTOR_COUNTS else (int, float)
        if isinstance(value, bool) or not isinstance(value, kinds) or value <= 0:
            what = 'integer' if key in EXECUTOR_COUNTS else 'number'
            raise ValueError(f'executor {name}: {key} must be a positive {what}, got {value!r}')
    refused = ec.get('refuse_task_types', [])
    if not isinstance(refused, list) or not all(isinstance(t, str) for t in refused):
        raise ValueError(f'executor {name}: refuse_task_types must be a list of strings, got {refused!r}')


def check_retry_backoff(rb):
    """Refuse a [retry_backoff] section without positive base_s and max_s, or with max_s below base_s."""
    for key in ('base_s', 'max_s'):
        value = rb.get(key)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or value <= 0:
            raise ValueError(f'retry_backoff: {key} must be a positive number, got {value!r}')
    if rb['max_s'] < rb['base_s']:
        raise ValueError(f"retry_backoff: max_s {rb['max_s']} is below base_s {rb['base_s']}")


def check_admission(adm):
    """Refuse an [admission] section whose urgent_max_step is not a step 1..5, whose urgent_sources is not a list of
    strings, whose urgent_reserve is not a non-negative integer or whose urgent_floor_mb is not a non-negative number."""
    if 'urgent_sources' not in adm:
        raise missing('admission.urgent_sources')
    step = adm['urgent_max_step']
    if isinstance(step, bool) or not isinstance(step, int) or not 1 <= step <= 5:
        raise ValueError(f'admission: urgent_max_step must be a step 1..5, got {step!r}')
    sources = adm['urgent_sources']
    if not isinstance(sources, list) or not all(isinstance(s, str) for s in sources):
        raise ValueError(f'admission: urgent_sources must be a list of strings, got {sources!r}')
    reserve = adm['urgent_reserve']
    if isinstance(reserve, bool) or not isinstance(reserve, int) or reserve < 0:
        raise ValueError(f'admission: urgent_reserve must be a non-negative integer, got {reserve!r}')
    floor = adm['urgent_floor_mb']
    if isinstance(floor, bool) or not isinstance(floor, (int, float)) or not floor >= 0:
        raise ValueError(f'admission: urgent_floor_mb must be a non-negative number of MiB, got {floor!r}')


class Connection(sqlite3.Connection):
    begin_retries = 0


def connect(db_path, begin_retries=0, owner_actor=None):
    """WAL connection with a 30 s busy timeout. begin_retries: extra BEGIN IMMEDIATE attempts after a
    busy timeout (nothing is written when BEGIN fails, so a retry is safe). The CLI passes 2; the
    dispatcher keeps 0 and simply retries on its next tick. owner_actor (config `owner_actor`) classifies
    the cards of a store older than card_class and is required when it has any."""
    Path(db_path).parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    conn = sqlite3.connect(db_path, timeout=BUSY_TIMEOUT_S, isolation_level=None, factory=Connection)
    conn.begin_retries = begin_retries
    conn.row_factory = sqlite3.Row
    conn.execute('PRAGMA journal_mode=WAL')
    conn.execute('PRAGMA synchronous=NORMAL')
    conn.execute('PRAGMA foreign_keys=ON')
    conn.execute(f'PRAGMA busy_timeout={BUSY_TIMEOUT_S * 1000}')
    conn.executescript(SCHEMA + RUNS_TABLE)
    card_columns = {r['name'] for r in conn.execute('PRAGMA table_info(cards)')}
    if 'acceptance_kind' not in card_columns:
        conn.execute("ALTER TABLE cards ADD COLUMN acceptance_kind TEXT NOT NULL DEFAULT 'command'")
    if 'judge_criteria' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN judge_criteria TEXT')
    if 'deny_tools' not in card_columns:
        conn.execute("ALTER TABLE cards ADD COLUMN deny_tools TEXT NOT NULL DEFAULT '[]'")
    if 'recheck_every' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN recheck_every REAL')
    if 'recheck_until' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN recheck_until REAL')
    if 'recheck_max' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN recheck_max INTEGER')
    if 'recheck_due' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN recheck_due REAL')
    if 'recheck_attempts' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN recheck_attempts INTEGER NOT NULL DEFAULT 0')
    if 'wait_reason' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN wait_reason TEXT')
    if 'assignee' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN assignee TEXT')
    if 'executor' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN executor TEXT')
    if 'model' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN model TEXT')
    if 'effort' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN effort TEXT')
    if 'panel' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN panel TEXT')
    if 'on_unavailable' not in card_columns:
        conn.execute("ALTER TABLE cards ADD COLUMN on_unavailable TEXT NOT NULL DEFAULT 'wait'")
    if 'executor_wait' not in card_columns:
        conn.execute('ALTER TABLE cards ADD COLUMN executor_wait TEXT')
    for name, decl in SERIES_COLUMNS.items():
        if name not in card_columns:
            conn.execute(f'ALTER TABLE cards ADD COLUMN {name} {decl}')
    if 'optional' not in {r['name'] for r in conn.execute('PRAGMA table_info(links)')}:
        conn.execute('ALTER TABLE links ADD COLUMN optional INTEGER NOT NULL DEFAULT 0')
    from .classification import migrate
    migrate(conn, owner_actor)
    indexes = conn.execute('PRAGMA index_list(runs)').fetchall()
    old_unique = any(i['origin'] == 'u' and
                     [c['name'] for c in conn.execute(f"PRAGMA index_info({i['name']})")] == ['card_id', 'round', 'role']
                     for i in indexes)
    columns = [r['name'] for r in conn.execute('PRAGMA table_info(runs)')]
    if old_unique:
        with tx(conn):
            conn.execute(RUNS_TABLE.replace('runs(', 'runs_new(', 1))
            names = ','.join(columns)
            conn.execute(f'INSERT INTO runs_new({names}) SELECT {names} FROM runs')
            conn.execute('DROP TABLE runs')
            conn.execute('ALTER TABLE runs_new RENAME TO runs')
    elif 'provider_failure' not in columns:
        conn.execute('ALTER TABLE runs ADD COLUMN provider_failure TEXT')
    run_columns = {r['name'] for r in conn.execute('PRAGMA table_info(runs)')}
    for name in ('member', 'report_file', 'job', 'report_sha256', 'report_tail'):
        if name not in run_columns:
            conn.execute(f'ALTER TABLE runs ADD COLUMN {name} TEXT')
    conn.execute("""CREATE UNIQUE INDEX IF NOT EXISTS runs_member_active_claim ON runs(card_id, round, role, COALESCE(member, ''))
                    WHERE status IN ('starting','running')""")
    conn.execute('DROP INDEX IF EXISTS runs_active_claim')
    conn.execute("""CREATE UNIQUE INDEX IF NOT EXISTS runs_reviewer_claim ON runs(card_id, round, role)
                    WHERE role='reviewer'""")
    conn.executescript(TICK_INDEXES)
    return conn


SLOW_TX_S = 5.0

_SYS_IOPRIO = {'x86_64': (251, 252), 'aarch64': (30, 31)}.get(os.uname().machine)
_IOPRIO_WHO_PROCESS, _IOPRIO_CLASS_SHIFT, _IOPRIO_CLASS_BE, _IOPRIO_CLASS_IDLE = 1, 13, 2, 3
try:
    _libc = ctypes.CDLL(None, use_errno=True)
except OSError:  # pragma: no cover
    _libc = None


def _ioprio(value=None):
    """Read or set this thread's I/O priority (Linux ioprio_get/ioprio_set on tid 0)."""
    if _libc is None or _SYS_IOPRIO is None:
        return None
    set_nr, get_nr = _SYS_IOPRIO
    if value is None:
        r = _libc.syscall(get_nr, _IOPRIO_WHO_PROCESS, 0)
        return None if r < 0 else r
    return _libc.syscall(set_nr, _IOPRIO_WHO_PROCESS, 0, value) == 0


@contextlib.contextmanager
def writer_io_priority():
    """Lift an idle-class thread to best-effort I/O while it holds the write lock.

    Head runners live in IOSchedulingClass=idle units. Under disk contention an idle-class reader can
    be starved for minutes; holding BEGIN IMMEDIATE meanwhile locks out the dispatcher and every CLI.
    Only this thread is lifted and only for the transaction; heads and other threads keep their class.
    Unprivileged processes may move idle -> best-effort."""
    prev = _ioprio()
    lifted = prev is not None and prev >> _IOPRIO_CLASS_SHIFT == _IOPRIO_CLASS_IDLE and \
        _ioprio((_IOPRIO_CLASS_BE << _IOPRIO_CLASS_SHIFT) | 4)
    try:
        yield lifted
    finally:
        if lifted:
            _ioprio(prev)


def begin_immediate(conn, retries=None):
    retries = getattr(conn, 'begin_retries', 0) if retries is None else retries
    for attempt in range(retries + 1):
        try:
            conn.execute('BEGIN IMMEDIATE')
            return
        except sqlite3.OperationalError as e:
            if 'locked' not in str(e) or attempt == retries:
                raise
            print(f'hive: hive.db is busy, retrying the write ({attempt + 1}/{retries})', file=sys.stderr)


@contextlib.contextmanager
def tx(conn, retries=None):
    """BEGIN IMMEDIATE: one writer; every read-check-write happens under it.

    Keep the body short and indexed: every other writer waits for it. A body that holds the lock
    longer than SLOW_TX_S is reported on stderr (the unit journal) with its caller."""
    with writer_io_priority():
        begin_immediate(conn, retries)
        started = time.monotonic()
        try:
            yield conn
        except BaseException:
            conn.execute('ROLLBACK')
            raise
        else:
            conn.execute('COMMIT')
        finally:
            held = time.monotonic() - started
            if held > SLOW_TX_S:
                frame = sys._getframe(2)
                print(f'hive: slow write transaction held hive.db for {held:.1f}s '
                      f'(pid {os.getpid()}, thread {threading.current_thread().name}, '
                      f'{Path(frame.f_code.co_filename).name}:{frame.f_lineno} {frame.f_code.co_name})',
                      file=sys.stderr, flush=True)


def event(conn, card_id, kind, data=None, actor='dispatcher', at=None):
    conn.execute('INSERT INTO events(card_id,at,kind,actor,data) VALUES(?,?,?,?,?)',
                 (card_id, at or time.time(), kind, actor,
                  json.dumps(data, ensure_ascii=False, default=str) if data is not None else None))


def cas_card(conn, card, new_state, **fields):
    """Move a card only if it is still in the state/version we read. Returns bool."""
    sets = ['state=?', 'version=version+1', 'updated_at=?']
    vals = [new_state, time.time()]
    for k, v in fields.items():
        sets.append(f'{k}=?')
        vals.append(v)
    vals += [card['id'], card['state'], card['version']]
    cur = conn.execute(f"UPDATE cards SET {', '.join(sets)} WHERE id=? AND state=? AND version=?", vals)
    return cur.rowcount == 1


def is_final(card):
    """A card is final when it reached a terminal state for a reason other than waiting."""
    return card['state'] in TERMINAL and card['wait_reason'] is None


def blockers(conn, card_id):
    """The card's blocked_by targets with their current states, in link order."""
    return [dict(r) for r in conn.execute("""SELECT c.id, c.state FROM links l JOIN cards c ON c.id=l.target_id
                                             WHERE l.card_id=? AND l.kind='blocked_by' ORDER BY l.rowid""",
                                          (card_id,))]


def children(conn, epic_id):
    """The epic's child cards with their states and whether each is optional, in link order."""
    return [dict(r, optional=bool(r['optional'])) for r in conn.execute(
        """SELECT c.id, c.state, c.wait_reason, l.optional FROM links l JOIN cards c ON c.id=l.card_id
           WHERE l.target_id=? AND l.kind='parent' ORDER BY l.rowid""", (epic_id,))]


def parent(conn, card_id):
    """The epic the card belongs to as {id, state, optional}, or None."""
    row = conn.execute("""SELECT c.id, c.state, l.optional FROM links l JOIN cards c ON c.id=l.target_id
                          WHERE l.card_id=? AND l.kind='parent'""", (card_id,)).fetchone()
    return dict(row, optional=bool(row['optional'])) if row else None


def instances(conn, series_id):
    """The series' instance cards in period order with their states and outcomes."""
    return [dict(r) for r in conn.execute(
        """SELECT c.id, c.series_period AS period, c.state, c.wait_reason, c.result, c.block_reason
           FROM links l JOIN cards c ON c.id=l.card_id WHERE l.target_id=? AND l.kind='series'
           ORDER BY c.series_period""", (series_id,))]


def instance_states(conn, series_id):
    """instances() without result/block_reason, read from the covering index only: the card rows of hundreds of
    old instances (wide goal/body with overflow pages) are not touched on every dispatcher tick."""
    return [dict(r) for r in conn.execute(
        """SELECT c.id, c.series_period AS period, c.state, c.wait_reason
           FROM links l JOIN cards c INDEXED BY cards_series_member ON c.id=l.card_id
           WHERE l.target_id=? AND l.kind='series' ORDER BY c.series_period""", (series_id,))]


def steers(conn, card_id):
    """All steer events of the card, oldest first; after_round = last worker round claimed before each."""
    rows = conn.execute("""SELECT s.id, s.data, COALESCE((SELECT max(json_extract(c.data, '$.round')) FROM events c
                               WHERE c.card_id=s.card_id AND c.kind='claimed' AND json_extract(c.data, '$.role')='worker'
                               AND c.id < s.id), 0) AS after_round
                           FROM events s WHERE s.card_id=? AND s.kind='steer' ORDER BY s.id""", (card_id,)).fetchall()
    return [dict(json.loads(r['data']), id=r['id'], after_round=r['after_round']) for r in rows]


def unseen_steers(conn, run_id):
    """Steers posted after this worker's claim, in journal order."""
    # The claim is always journaled on the run's own card: search that card's events, not the whole journal.
    claim = conn.execute("""SELECT id, card_id FROM events WHERE card_id=(SELECT card_id FROM runs WHERE id=?)
                            AND kind='claimed' AND json_extract(data, '$.run')=? AND json_extract(data, '$.role')='worker'""",
                         (run_id, run_id)).fetchone()
    delivered = live_steers(conn, run_id)
    return [s for s in steers(conn, claim['card_id']) if s['id'] > claim['id'] and s['id'] not in delivered]


def live_steers(conn, run_id):
    """Only vendor-confirmed input counts as delivered, never a successful pipe write."""
    return [json.loads(e['data'])['steer'] for e in conn.execute(
        "SELECT data FROM events WHERE card_id=(SELECT card_id FROM runs WHERE id=?) "
        "AND kind='steer_live' AND json_extract(data, '$.run')=? ORDER BY id", (run_id, run_id))]


def live_failure(conn, run_id):
    row = conn.execute("SELECT data FROM events WHERE card_id=(SELECT card_id FROM runs WHERE id=?) "
                       "AND kind='steer_live_failed' AND json_extract(data, '$.run')=? ORDER BY id DESC LIMIT 1",
                       (run_id, run_id)).fetchone()
    return json.loads(row['data'])['error'] if row else None


def run_live_input(run):
    """The immutable launch job is authoritative, including for pre-upgrade runners."""
    try:
        return bool(json.loads((Path(run['run_dir']) / 'job.json').read_text()).get('live_input'))
    except (OSError, ValueError, TypeError):
        return False


def meta_get(conn, key):
    row = conn.execute('SELECT value, at FROM meta WHERE key=?', (key,)).fetchone()
    return (json.loads(row['value']), row['at']) if row else (None, None)


def meta_set(conn, key, value):
    conn.execute('INSERT INTO meta(key,value,at) VALUES(?,?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value, at=excluded.at',
                 (key, json.dumps(value, ensure_ascii=False, default=str), time.time()))
