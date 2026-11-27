"""Hive engine tests with fake heads (no model calls, no live DB/inbox)."""
import json
import datetime as dt
import errno
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT))
from hive import api, liveness, store, heads, dispatcher  # noqa: E402

REAL_SCHEDULE_GATE = dispatcher.Dispatcher.schedule_gate

FAKE = Path(__file__).with_name('fake_head.py')
FAKE_DOTS = Path(__file__).with_name('fake_dots.py')
FAKE_ULEI_TG = Path(__file__).with_name('fake_ulei_tg.py')
FAKE_INBOX_LIBRARY = Path(__file__).with_name('fake_inbox_library')


# Test-owned hive config: tests rely on these values, never on a deployment's config.
TEST_CONFIG = """
db = "{tmp}/hive.db"
workspaces = "{tmp}/cards"
inbox = "{tmp}/inbox"
concurrency = 2
max_rounds = 2
heartbeat_stale_s = 2
# Match production: a cold Python interpreter can need several seconds on the host.
spawn_grace_s = 90
gate_timeout_s = 300
held_long_s = 259200
pick_window = 12
launcher = "popen"
owner_actor = "owner"
owner_tz = "Europe/Athens"
delivery = "drop"
inbox_library = "{library}"
results_dir = "{tmp}/out"
card_show_command = "hive.py show"
script_env_file = "{tmp}/agent/.env"
steward_dir = "{tmp}/steward"
rotation_log = "{tmp}/reports/worktree-rotation.jsonl"
gateway_socket = "{tmp}/gw.sock"

[timeouts]
1 = 1200
2 = 1800
3 = 3600
4 = 7200
5 = 10800

[ladder.5]
claude = {{ model = "claude-opus-5-5", effort = "xhigh" }}
codex  = {{ model = "gpt-6-astra",      effort = "xhigh" }}
[ladder.4]
claude = {{ model = "claude-opus-5-5",  effort = "high" }}
codex  = {{ model = "gpt-6.1-sol",      effort = "high" }}
[ladder.3]
claude = {{ model = "claude-opus-5-5",  effort = "medium" }}
codex  = {{ model = "gpt-6.1-sol",      effort = "medium" }}
[ladder.2]
claude = {{ model = "claude-sonnet-5-5", effort = "high" }}
codex  = {{ model = "gpt-6.1-terra",     effort = "medium" }}
[ladder.1]
claude = {{ model = "claude-sonnet-5-5", effort = "medium" }}
codex  = {{ model = "gpt-6-luna",        effort = "medium" }}

[vendors.claude]
live_input = false
weight = 1.0
daily_runs = 100
binary = ["{fake}"]
home = "{tmp}"
tag_launcher = true

[vendors.codex]
live_input = false
weight = 1.0
daily_runs = 100
binary = ["{fake}"]
home = "{tmp}"
tag_launcher = false

[executors.dots]
command = ["{tmp}/bin/fakedots"]
poll_s = 60
call_timeout_s = 30
timeout_s = 600
max_parallel = 2
daily_sends = 20
cooldown_s = 300
refuse_task_types = ["код", "code"]

[budget]
low_weekly_left_pct = 20
low_max_step = 2
limits_ttl_s = 900

[head_env]
LIBRARY_PATH = "{tmp}/library"

[gate_env]
HOME = "{tmp}"
PATH = "{tmp}/.local/bin:/usr/local/bin:/usr/bin:/bin"
LIBRARY_PATH = "{tmp}/library"
HIVE_CONFIG = "{tmp}/config.toml"

[script_env]
HOME = "{tmp}"
PATH = "{tmp}/.local/bin:/usr/local/bin:/usr/bin:/bin"
AGENT_HOME = "{tmp}/agent"

[texts]
workspace_context = "{texts}/workspace-context.md"
report_request = "{texts}/report-request.txt"
series_news_note = "{texts}/series-news-note.txt"
result_note = "{texts}/result-note.txt"
missed_note = "{texts}/missed-note.txt"
steward_prompt = "{texts}/steward-prompt.txt"
steward_alert = "{texts}/steward-alert.txt"

[admission]
urgent_sources = ["cron:000000000001", "cron:000000000002"]

[judge]
max_tokens = 1024
timeout_s = 30
retry_s = 300
routes = {{ 1 = "judge-s1", 2 = "judge-s2", 3 = "judge-s3", 4 = "judge-s4", 5 = "judge-s5" }}

[steward]
route = "steward-test"
max_tokens = 2048
timeout_s = 30

[reminders]
tg_binary = "{tmp}/bin/fake-ulei-tg"
tg_config = "{tmp}/ulei.toml"
"""


@pytest.fixture
def env(tmp_path, monkeypatch, gateway):
    fake = tmp_path / 'bin' / 'fakehead'
    fake.parent.mkdir()
    fake.write_text(f'#!/bin/sh\nexec {sys.executable} {FAKE} "$@"\n')
    fake.chmod(0o755)
    dots = tmp_path / 'bin' / 'fakedots'
    dots.write_text(f'#!/bin/sh\nexec {sys.executable} {FAKE_DOTS} {tmp_path / "dots"} "$@"\n')
    dots.chmod(0o755)
    tg = tmp_path / 'bin' / 'fake-ulei-tg'
    tg.write_text(f'#!/bin/sh\nexec {sys.executable} {FAKE_ULEI_TG} {tmp_path / "tg"} "$@"\n')
    tg.chmod(0o755)
    (tmp_path / 'tg').mkdir()
    config = tmp_path / 'config.toml'
    config.write_text(TEST_CONFIG.format(tmp=tmp_path, fake=fake, library=FAKE_INBOX_LIBRARY, texts=ROOT / 'texts'))
    monkeypatch.setenv('HIVE_CONFIG', str(config))
    monkeypatch.delenv('HIVE_DB', raising=False)
    cfg = store.load_config(config)
    cfg['budget']['limits_command'] = None
    # Pool-count tests use the full ceilings; the urgent reserve tests set their own.
    cfg['admission']['urgent_reserve'] = 0
    inbox_dir = Path(cfg['inbox'])
    conn = store.connect(cfg['db'])
    inbox_dir.mkdir(mode=0o700)
    d = dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {})
    # State-machine tests run gate completion inline; detached execution has dedicated integration tests.
    monkeypatch.setattr(dispatcher.Dispatcher, 'schedule_gate', lambda self, card, mode: getattr(self, mode)(card))
    return cfg, conn, d


def mk(conn, title, review=False, step=1, **kw):
    cid, _ = api.create(conn, title=title, goal='make out.txt', acceptance='test -s out.txt', step=step,
                        source='system:test', created_by='secretary', review=review, **kw)
    return cid


def run_until(d, conn, cid, states=('done', 'blocked'), timeout=40):
    end = time.time() + timeout
    while time.time() < end:
        d.tick()
        st = conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()['state']
        if st in states:
            return st
        time.sleep(0.2)
    raise AssertionError(f'card {cid} stuck in {st}')


def kinds(conn, cid):
    return [r['kind'] for r in conn.execute('SELECT kind FROM events WHERE card_id=? ORDER BY id', (cid,))]


# ── create / validation / idempotency ─────────────────────────────────────────

def test_create_validates(env):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='готово', step=1, source='system:test', created_by='secretary')
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='test -f (', step=1, source='system:test', created_by='secretary')
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=7, source='system:test', created_by='secretary')
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary', kind='weird')
    cid = mk(conn, '[ok] fine')
    c = api.get(conn, cid)
    assert c['state'] == 'created' and c['kind'] == 'task' and c['lane'] == 'task' and c['review'] == 0


def test_command_acceptance_contract_defaults(env):
    _, conn, _ = env
    card = api.get(conn, mk(conn, '[ok] command contract'))
    assert card['acceptance_kind'] == 'command'
    assert card['judge_criteria'] is None
    assert card['deny_tools'] == []


def test_tool_denial_contract_validates_and_round_trips(env):
    _, conn, _ = env
    cid, created = api.create(conn, title='[ok] restricted', goal='g', acceptance='true', step=1,
                              source='system:test', created_by='secretary', deny_tools=('write', 'browser'))
    assert created
    assert api.get(conn, cid)['deny_tools'] == ['browser', 'write']
    for policy in ('browser', None, ['browser', 'browser'], ['shell'], ['browser', 1]):
        with pytest.raises(api.CardError):
            api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                       deny_tools=policy)


def test_judge_acceptance_contract_round_trips(env):
    _, conn, _ = env
    cid, created = api.create(conn, title='[ok] judge contract', goal='g', acceptance='true', step=1,
                              source='system:test', created_by='secretary', acceptance_kind='judge',
                              judge_criteria='The output explains the result accurately.')
    card = api.get(conn, cid)
    assert created
    assert card['acceptance_kind'] == 'judge'
    assert card['judge_criteria'] == 'The output explains the result accurately.'


def test_judge_acceptance_contract_validates(env):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                   acceptance_kind='other')
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                   acceptance_kind='judge', judge_criteria='  ')


@pytest.mark.parametrize('criteria', [b'criterion', 1])
def test_judge_acceptance_contract_rejects_non_text_criteria(env, criteria):
    _, conn, _ = env
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                   acceptance_kind='judge', judge_criteria=criteria)


def test_connect_migrates_legacy_card_acceptance_contract(tmp_path):
    db = tmp_path / 'legacy.db'
    conn = sqlite3.connect(db)
    conn.execute("""CREATE TABLE cards(
        id TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'task', lane TEXT NOT NULL DEFAULT 'task',
        title TEXT NOT NULL, goal TEXT NOT NULL, acceptance TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
        step INTEGER NOT NULL, review INTEGER NOT NULL DEFAULT 0, task_type TEXT NOT NULL DEFAULT 'general',
        source TEXT NOT NULL, created_by TEXT NOT NULL, idem_key TEXT UNIQUE, priority INTEGER NOT NULL DEFAULT 0,
        not_before REAL, state TEXT NOT NULL, round INTEGER NOT NULL DEFAULT 0, version INTEGER NOT NULL DEFAULT 0,
        workdir TEXT, block_reason TEXT, result TEXT, feedback TEXT, created_at REAL NOT NULL,
        updated_at REAL NOT NULL, finished_at REAL)""")
    conn.execute("""INSERT INTO cards(id,kind,lane,title,goal,acceptance,step,source,created_by,state,created_at,updated_at)
                    VALUES('legacy','task','task','old card','g','true',1,'s','secretary','created',1,1)""")
    conn.commit()
    conn.close()
    migrated = store.connect(db, owner_actor='owner')
    card = api.get(migrated, 'legacy')
    assert card['acceptance_kind'] == 'command'
    assert card['judge_criteria'] is None
    assert card['deny_tools'] == []
    assert card['recheck_policy'] is None
    assert card['recheck_state'] is None
    assert card['blocked_by'] == [] and card['block_reasons'] == []
    dependent = api.create(migrated, title='new', goal='g', acceptance='true', step=1, source='system:test',
                           created_by='secretary', blocked_by=['legacy'])[0]
    reopened = store.connect(db, owner_actor='owner')
    assert api.get(reopened, dependent)['blocked_by'] == [{'id': 'legacy', 'state': 'created'}]


def test_recheck_policy_round_trips_with_initial_due_state(env, monkeypatch):
    _, conn, d = env
    monkeypatch.setattr(api.time, 'time', lambda: 1000)
    d.clock = lambda: 1000
    cid, created = api.create(conn, title='[ok] deferred recheck', goal='g', acceptance='true', step=1,
                              source='system:test', created_by='secretary', not_before=1200, recheck_every=60,
                              recheck_until=1800, recheck_max=3)
    card = api.get(conn, cid)
    assert created
    assert card['recheck_policy'] == {'every': 60.0, 'until': 1800, 'max': 3}
    assert card['recheck_state'] == {'due': 1060, 'attempts': 0}
    d.tick()
    assert api.get(conn, cid)['state'] == 'created'
    assert card['runs'] == [] and card['gates'] == []


@pytest.mark.parametrize('schedule', [
    {'recheck_until': 1200},
    {'recheck_every': 60},
    {'recheck_every': 0, 'recheck_max': 1},
    {'recheck_every': 60, 'recheck_until': 1000, 'recheck_max': 1},
    {'recheck_every': 60, 'recheck_max': 0},
    {'recheck_every': 60, 'recheck_until': 1200, 'not_before': 1200, 'recheck_max': 1},
])
def test_recheck_policy_rejects_invalid_or_incomplete_schedules(env, monkeypatch, schedule):
    _, conn, _ = env
    monkeypatch.setattr(api.time, 'time', lambda: 1000)
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary', **schedule)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def test_recheck_policy_rejects_deadline_before_first_due(env, monkeypatch):
    _, conn, _ = env
    monkeypatch.setattr(api.time, 'time', lambda: 1000)
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                   recheck_every=60, recheck_until=1001, recheck_max=1)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def completed_scheduled_card(conn, tmp_path, *, acceptance='true', **schedule):
    cid, _ = api.create(conn, title='scheduled card', goal='g', acceptance=acceptance, step=1,
                        source='system:test', created_by='secretary', **schedule)
    conn.execute("""UPDATE cards SET state='done', round=1, workdir=?, result='gate passed',
                    finished_at=?, version=version+1 WHERE id=?""", (str(tmp_path), api.time.time(), cid))
    return cid


@pytest.mark.parametrize('recheck', [False, True])
@pytest.mark.parametrize('bus_env', [
    {},
    {'XDG_RUNTIME_DIR': '/run/user/1234'},
    {'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/run/user/1234/bus'},
    {'XDG_RUNTIME_DIR': '/run/user/1234', 'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/run/user/1234/bus'},
])
def test_gates_preserve_user_bus_without_inheriting_secrets(env, monkeypatch, tmp_path, recheck, bus_env):
    _, conn, d = env
    for key in ('XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'):
        monkeypatch.delenv(key, raising=False)
    for key, value in bus_env.items():
        monkeypatch.setenv(key, value)
    for key in ('GITHUB_TOKEN', 'TELEGRAM_BOT_TOKEN', 'AGENT_PROFILE'):
        monkeypatch.setenv(key, 'must-not-reach-gate')
    command = ('printf "%s\\n" "${XDG_RUNTIME_DIR-unset}" "${DBUS_SESSION_BUS_ADDRESS-unset}" '
               '"${GITHUB_TOKEN-unset}" "${TELEGRAM_BOT_TOKEN-unset}" "${AGENT_PROFILE-unset}"')
    if recheck:
        cid = completed_scheduled_card(conn, tmp_path, acceptance=command, recheck_every=60, recheck_max=1)
        conn.execute('UPDATE cards SET recheck_due=1 WHERE id=?', (cid,))
    else:
        cid = mk_epic(conn, 'user service check', acceptance=command)
        conn.execute("UPDATE cards SET state='check', workdir=? WHERE id=?", (str(tmp_path), cid))
    card = conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()
    assert (d.recheck(card) if recheck else d.gate(card)) == 1
    result = api.get(conn, cid)
    assert result['state'] == 'done'
    assert result['gates'][-1]['passed'] == 1
    assert result['gates'][-1]['output'].splitlines() == [
        bus_env.get('XDG_RUNTIME_DIR', 'unset'),
        bus_env.get('DBUS_SESSION_BUS_ADDRESS', 'unset'),
        'unset', 'unset', 'unset',
    ]
    assert result['runs'] == []


def test_due_recheck_records_attempt_and_schedules_next_without_heads(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = [1000]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    d.clock = lambda: now[0]
    cid = completed_scheduled_card(conn, tmp_path, recheck_every=60, recheck_max=3)

    now[0] = 1060
    assert d.advance() == 1

    card = api.get(conn, cid)
    assert card['state'] == 'done'
    assert card['recheck_state'] == {'due': 1120.0, 'attempts': 1}
    assert [(gate['round'], gate['passed']) for gate in card['gates']] == [(2, 1)]
    assert card['runs'] == []
    assert card['events'][-1]['kind'] == 'recheck'
    assert card['events'][-1]['data'] == {'attempt': 1, 'exit': 0, 'passed': True, 'next_due': 1120.0}


@pytest.mark.parametrize('schedule', [{'recheck_max': 1}, {'recheck_until': 1120}])
def test_final_due_recheck_marks_done_and_does_not_repeat(env, monkeypatch, tmp_path, schedule):
    _, conn, d = env
    now = [1000]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    d.clock = lambda: now[0]
    cid = completed_scheduled_card(conn, tmp_path, recheck_every=60, **schedule)

    now[0] = 1060
    assert d.advance() == 1
    card = api.get(conn, cid)
    assert card['state'] == 'done'
    assert card['recheck_state'] == {'due': None, 'attempts': 1}
    assert card['result'] == 'recheck passed'
    assert card['runs'] == []
    assert d.advance() == 0
    assert api.get(conn, cid)['recheck_state'] == {'due': None, 'attempts': 1}


def test_failed_due_recheck_blocks_without_retry_heads(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = [1000]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    d.clock = lambda: now[0]
    cid = completed_scheduled_card(conn, tmp_path, acceptance='printf failed >&2; exit 4',
                                   recheck_every=60, recheck_max=2)

    now[0] = 1060
    assert d.advance() == 1

    card = api.get(conn, cid)
    assert card['state'] == 'blocked'
    assert card['recheck_state'] == {'due': None, 'attempts': 1}
    assert card['block_reason'] == 'recheck exit 4'
    assert card['gates'][0]['exit_code'] == 4 and 'failed' in card['gates'][0]['output']
    assert card['runs'] == []
    assert 'retry' not in kinds(conn, cid)


def test_recheck_skips_before_due_at_deadline_and_while_initial_card_is_held(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = [1000]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    d.clock = lambda: now[0]
    completed = completed_scheduled_card(conn, tmp_path, recheck_every=60, recheck_until=1120)
    held, _ = api.create(conn, title='held schedule', goal='g', acceptance='true', step=1,
                         source='system:test', created_by='secretary', not_before=1200,
                         recheck_every=60, recheck_max=1)

    now[0] = 1059
    assert d.advance() == 0
    now[0] = 1120
    assert d.advance() == 0

    card = api.get(conn, completed)
    assert card['recheck_state'] == {'due': 1060.0, 'attempts': 0}
    assert card['gates'] == [] and card['runs'] == []
    held_card = api.get(conn, held)
    assert held_card['state'] == 'created'
    assert held_card['recheck_state'] == {'due': 1060.0, 'attempts': 0}
    now[0] = 1200
    assert d.advance() == 1
    assert api.get(conn, held)['state'] == 'queued'
    assert api.get(conn, held)['runs'] == []


def test_idempotency_race(env):
    cfg, _, _ = env
    results, barrier = [], threading.Barrier(8)

    def worker():
        c = store.connect(cfg['db'])
        barrier.wait()
        results.append(api.create(c, title='[ok] same', goal='g', acceptance='true', step=1, source='system:test',
                                  created_by='secretary', idem_key='k-1'))
    ts = [threading.Thread(target=worker) for _ in range(8)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    ids = {r[0] for r in results}
    assert len(ids) == 1 and sum(1 for r in results if r[1]) == 1
    conn = store.connect(cfg['db'])
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 1


# ── state machine / gate / review ───────────────────────────────────────────

def test_happy_path_gate_only(env):
    _, conn, d = env
    cid = mk(conn, '[ok] happy')
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 1 and len(c['runs']) == 1 and c['gates'][0]['passed'] == 1
    r = c['runs'][0]
    assert r['status'] == 'exited' and r['model'] in ('claude-sonnet-5-5', 'gpt-6-luna') and r['effort'] == 'medium'
    assert r['prompt'].startswith('# Card') and r['report'].startswith('round 1') and r['duration_s'] is not None
    assert r['tokens_in'] is not None or r['vendor'] == 'codex'
    k = kinds(conn, cid)
    assert k[:3] == ['created', 'queued', 'claimed'] and 'gate' in k and 'done' in k and k[-1] == 'secretary_event'
    assert (Path(c['workdir']) / 'CLAUDE.md').is_file() and (Path(c['workdir']) / 'AGENTS.md').is_file()


def test_retry_then_pass(env):
    _, conn, d = env
    cid = mk(conn, '[round2] second time lucky')
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [g['passed'] for g in c['gates']] == [0, 1]
    assert 'retry' in kinds(conn, cid)
    assert 'Previous round failed' in c['runs'][1]['prompt']


def test_provider_failure_uses_other_vendor_in_same_round(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [provider429] alternate vendor')
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    first, second = workers(c)
    assert c['round'] == 1 and [first['round'], second['round']] == [1, 1]
    assert first['vendor'] != second['vendor'] and first['provider_failure'] == 'http_429'
    assert second['provider_failure'] is None and first['status'] == 'exited' and first['exit_code'] == 1
    assert [g['passed'] for g in c['gates']] == [1]
    assert Path(first['run_dir'], 'head.log').read_text().strip() == 'HTTP 429: too many requests'
    assert [e['data']['run'] for e in c['events'] if e['kind'] == 'provider_failover'] == [first['id']]
    allowed, why = heads.allowed_vendors(conn, cfg, 1, {}, d.clock(), tag=None)
    assert allowed == [second['vendor']] and 'cooldown' in why[first['vendor']]


def test_provider_outage_waits_and_recovers_in_same_round(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 10}
    now = [1000.0]
    d.clock = lambda: now[0]
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [provideroutage] recover', step=2)

    d.tick()
    d.tick()
    c = api.get(conn, cid)
    first, second = workers(c)
    assert c['state'] == 'queued' and c['round'] == 1 and c['step'] == 2
    assert [r['round'] for r in workers(c)] == [1, 1]
    assert first['vendor'] != second['vendor']
    assert all(r['provider_failure'] == 'http_429' and r['handled'] for r in workers(c))
    assert [e['data']['run'] for e in c['events'] if e['kind'] == 'provider_outage_wait'] == [second['id']]
    assert 'retry' not in kinds(conn, cid)
    episode, _ = store.meta_get(conn, 'provider_outage_episode')
    assert isinstance(episode, str)
    outage_id = f'hive:provider:{episode}:outage'
    outage = d.adapter().inbox.get(outage_id)
    assert outage is not None
    outage_payload = json.loads(d.adapter().inbox.packets[outage_id]['payload'])
    assert {k: outage_payload[k] for k in ('source', 'episode', 'transition', 'card_id')} == {
        'source': 'hive', 'episode': episode, 'transition': 'outage', 'card_id': cid}
    assert set(outage_payload['vendors']) == {first['vendor'], second['vendor']}

    waiting_card = mk(conn, '[ok] another waiting card', step=2)

    for _ in range(3):
        d.tick()
    waiting = api.get(conn, cid)
    assert waiting['state'] == 'queued' and waiting['round'] == 1 and len(workers(waiting)) == 2
    assert conn.execute("SELECT count(*) FROM outbox WHERE card_id=? AND kind='task_blocked'", (cid,)).fetchone()[0] == 0
    assert 'retry' not in kinds(conn, cid)
    assert api.get(conn, waiting_card)['state'] == 'queued'
    assert conn.execute("SELECT count(*) FROM outbox WHERE kind='provider_outage'").fetchone()[0] == 1
    assert store.meta_get(conn, 'provider_outage_episode')[0] == episode

    now[0] += 10
    assert tick_until_terminal(d, conn, cid) == 'done'
    recovered = api.get(conn, cid)
    assert recovered['round'] == 1 and recovered['step'] == 2
    assert [r['round'] for r in workers(recovered)] == [1, 1, 1]
    assert workers(recovered)[2]['status'] == 'exited'
    assert [e['data']['round'] for e in recovered['events'] if e['kind'] == 'claimed'] == [1, 1, 1]
    assert conn.execute("SELECT count(*) FROM outbox WHERE card_id=? AND kind='task_blocked'", (cid,)).fetchone()[0] == 0
    recovery_id = f'hive:provider:{episode}:recovery'
    assert d.adapter().inbox.get(recovery_id) is not None
    recovery = json.loads(d.adapter().inbox.packets[recovery_id]['payload'])
    assert recovery['episode'] == episode and recovery['transition'] == 'recovery'
    assert recovery['vendors'] == [workers(recovered)[2]['vendor']]
    assert store.meta_get(conn, 'provider_outage_episode')[0] is None
    for _ in range(3):
        d.tick()
    assert conn.execute("SELECT count(*) FROM outbox WHERE kind='provider_recovery'").fetchone()[0] == 1

    later = mk(conn, '[ok] [provideroutage] later', step=2)
    d.tick()
    d.tick()
    next_episode, _ = store.meta_get(conn, 'provider_outage_episode')
    assert next_episode and next_episode != episode
    assert d.adapter().inbox.get(f'hive:provider:{next_episode}:outage') is not None
    now[0] += 10
    assert tick_until_terminal(d, conn, later) == 'done'
    assert d.adapter().inbox.get(f'hive:provider:{next_episode}:recovery') is not None
    assert [tuple(r) for r in conn.execute("SELECT kind, count(*) FROM outbox WHERE kind LIKE 'provider_%' GROUP BY kind")] == [
        ('provider_outage', 2), ('provider_recovery', 2)]


def test_provider_cooldown_backoff_cap_and_restart(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 25}
    cfg['vendors']['codex']['disabled'] = True
    now = [1000.0]
    d.clock = lambda: now[0]
    run_heads_inline(cfg, d)

    for failures, delay in ((1, 10), (2, 20), (3, 25)):
        cid = mk(conn, f'[provider429] failure {failures}')
        assert tick_until_terminal(d, conn, cid) == 'blocked'
        state, _ = store.meta_get(conn, 'provider_cooldown:claude')
        assert state == {'failures': failures, 'until': now[0] + delay}
        reopened = store.connect(cfg['db'])
        fresh = dispatcher.Dispatcher(cfg, reopened, limits_fetcher=lambda c: {}, clock=lambda: now[0])
        allowed, why = heads.allowed_vendors(fresh.conn, cfg, 1, {}, fresh.clock(), tag=None)
        assert 'claude' not in allowed and 'cooldown' in why['claude']
        now[0] += delay
        allowed, _ = heads.allowed_vendors(fresh.conn, cfg, 1, {}, fresh.clock(), tag=None)
        assert allowed == ['claude']
        reopened.close()
    assert conn.execute("SELECT count(*) FROM outbox WHERE kind LIKE 'provider_%'").fetchone()[0] == 0


def test_successful_worker_clears_provider_cooldown(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 25}
    cfg['vendors']['codex']['disabled'] = True
    now = [1000.0]
    d.clock = lambda: now[0]
    run_heads_inline(cfg, d)
    failed = mk(conn, '[provider429] first attempt')
    assert tick_until_terminal(d, conn, failed) == 'blocked'
    now[0] += 10
    succeeded = mk(conn, '[ok] recovered vendor')
    assert tick_until_terminal(d, conn, succeeded) == 'done'
    assert workers(api.get(conn, succeeded))[0]['vendor'] == 'claude'
    assert store.meta_get(conn, 'provider_cooldown:claude')[0] is None


def test_existing_run_table_keeps_history_and_one_active_claim(env):
    cfg, conn, _ = env
    cid = mk(conn, '[ok] existing database')
    conn.execute('DROP TABLE runs')
    conn.execute("""CREATE TABLE runs (
        id INTEGER PRIMARY KEY AUTOINCREMENT, card_id TEXT NOT NULL, round INTEGER NOT NULL,
        role TEXT NOT NULL, vendor TEXT NOT NULL, model TEXT NOT NULL, effort TEXT NOT NULL,
        step INTEGER NOT NULL, prompt TEXT NOT NULL, status TEXT NOT NULL,
        started_at REAL NOT NULL, UNIQUE(card_id, round, role))""")
    conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                    VALUES(?,1,'worker','claude','model','medium',1,'prompt','exited',?)""", (cid, time.time()))
    conn.close()
    migrated = store.connect(cfg['db'])
    migrated.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                        VALUES(?,1,'worker','codex','model','medium',1,'prompt','starting',?)""", (cid, time.time()))
    assert [r['vendor'] for r in api.get(migrated, cid)['runs']] == ['claude', 'codex']
    with pytest.raises(sqlite3.IntegrityError):
        migrated.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                            VALUES(?,1,'worker','claude','model','medium',1,'prompt','starting',?)""", (cid, time.time()))


def test_ordinary_gate_failure_consumes_next_round(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[round2] task needs another pass')
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [r['round'] for r in workers(c)] == [1, 2]
    assert all(r['provider_failure'] is None for r in workers(c))
    assert [g['passed'] for g in c['gates']] == [0, 1]


def test_two_failures_block_and_event(env):
    _, conn, d = env
    cid = mk(conn, '[never] hopeless')
    assert run_until(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    assert c['round'] == 2 and len(c['runs']) == 2 and ('gate' in c['block_reason'] or 'identical worker reports' in c['block_reason'])
    ob = c['outbox'][0]
    assert ob['kind'] == 'task_blocked' and ob['published_at']
    ev = d.adapter().inbox.get(ob['event_id'])
    assert ev and json.loads(json.loads(ev['packet'])['payload'])['state'] == 'blocked'


def test_worker_nonzero_exit_counts_as_failure(env):
    cfg, conn, d = env
    cid = mk(conn, '[exit1] broken head')
    assert run_until(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    assert c['block_reason'] == 'worker exited: exit 1' and c['gates'] == []
    assert c['round'] == cfg['max_rounds']
    assert kinds(conn, cid).count('retry') == cfg['max_rounds'] - 1


def test_review_cross_vendor_pass(env):
    _, conn, d = env
    cid = mk(conn, '[ok] reviewed', review=True)
    assert run_until(d, conn, cid) == 'done'
    runs = api.get(conn, cid)['runs']
    w, r = [x for x in runs if x['role'] == 'worker'][0], [x for x in runs if x['role'] == 'reviewer'][0]
    assert w['vendor'] != r['vendor'] and r['verdict'] == 'PASS'
    assert 'review_requested' in kinds(conn, cid)


def test_review_fail_twice_blocks(env):
    _, conn, d = env
    cid = mk(conn, '[reviewfail] reviewer hates it', review=True)
    assert run_until(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [r['verdict'] for r in c['runs'] if r['role'] == 'reviewer'] == ['FAIL', 'FAIL']
    assert 'missing thing' in c['runs'][2]['prompt']  # reviewer feedback reaches round 2 worker


def test_review_fail_notes_stay_out_of_the_next_round(env):
    _, conn, d = env
    cid = mk(conn, '[reviewfail][reviewnotes] reviewer adds a note', review=True)
    assert run_until(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    reviews = [r for r in c['runs'] if r['role'] == 'reviewer']
    assert all('## Notes\nminor wording' in r['report'] for r in reviews)  # the author still finds the notes
    [second] = [r for r in c['runs'] if r['role'] == 'worker' and r['round'] == 2]
    assert 'missing thing' in second['prompt'] and 'minor wording' not in second['prompt']


def test_reviewer_fails_with_at_most_three_items_naming_their_criteria_and_keeps_notes_apart(env):
    _, conn, _ = env
    card = api.get(conn, mk(conn, '[ok] reviewer rule', review=True))
    text = heads.reviewer_prompt(card, 1, 'report', {'exit_code': 0, 'output': ''})
    for rule in ('A FAIL lists at most 3 essential items', 'Each item names the criterion',
                 'nothing\nminor goes in them', 'under a heading `## Notes`',
                 'not to the next worker, and they are never a reason for a FAIL'):
        assert rule in text
    assert 'the missing work first' not in text


def test_review_fail_then_pass(env):
    _, conn, d = env
    cid = mk(conn, '[reviewround2] fixed after review', review=True)
    assert run_until(d, conn, cid) == 'done'
    assert api.get(conn, cid)['round'] == 2


def test_judge_review_delivers_persisted_criteria_and_uses_other_vendor(env, monkeypatch, capsys):
    cfg, conn, d = env
    criteria = 'The result names its evidence.\nThe result explains its conclusion.'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] judge review', '--goal', 'make out.txt',
                    '--accept-review', criteria, '--step', '1', '--source', 'manual', '--kind', 'system',
                    '--created-by', 'secretary', '--review') == 0
    cid = capsys.readouterr().out.split()[0]
    run_heads_inline(cfg, d)

    assert tick_until_terminal(d, conn, cid) == 'done'
    card = api.get(conn, cid)
    worker, reviewer = card['runs']
    assert worker['role'] == 'worker' and reviewer['role'] == 'reviewer'
    assert worker['vendor'] != reviewer['vendor']
    assert reviewer['verdict'] == 'PASS' and reviewer['report'] == 'VERDICT: PASS\nlooks right\n'
    assert '## Persisted judge criteria\n```text\n' + criteria + '\n```' in reviewer['prompt']
    assert 'every persisted judge criterion is satisfied' not in reviewer['prompt']
    assert 'work a criterion requires was not done' in reviewer['prompt']
    assert 'A gap only in how the report presents finished work goes under `## Notes` after `VERDICT: PASS`' in reviewer['prompt']


def test_judge_review_fail_feedback_retries_then_passes(env, monkeypatch, capsys):
    cfg, conn, d = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[reviewround2] judge retry', '--goal', 'make out.txt',
                    '--accept-review', 'The result explains the outcome.', '--step', '1', '--source', 'manual', '--kind', 'system',
                    '--created-by', 'secretary', '--review') == 0
    cid = capsys.readouterr().out.split()[0]
    run_heads_inline(cfg, d)

    assert tick_until_terminal(d, conn, cid) == 'done'
    card = api.get(conn, cid)
    workers_by_round = {run['round']: run for run in card['runs'] if run['role'] == 'worker'}
    reviews = [run for run in card['runs'] if run['role'] == 'reviewer']
    assert [run['verdict'] for run in reviews] == ['FAIL', 'PASS']
    assert all(run['vendor'] != workers_by_round[run['round']]['vendor'] for run in reviews)
    assert reviews[0]['report'] == 'VERDICT: FAIL\nmissing thing\n'
    assert 'missing thing' in workers_by_round[2]['prompt']


def test_judge_review_repeated_failures_block_with_visible_reasons(env, monkeypatch, capsys):
    cfg, conn, d = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[reviewfail] judge blocked', '--goal', 'make out.txt',
                    '--accept-review', 'The result explains the outcome.', '--step', '1', '--source', 'manual', '--kind', 'system',
                    '--created-by', 'secretary', '--review') == 0
    cid = capsys.readouterr().out.split()[0]
    run_heads_inline(cfg, d)

    assert tick_until_terminal(d, conn, cid) == 'blocked'
    card = api.get(conn, cid)
    reviews = [run for run in card['runs'] if run['role'] == 'reviewer']
    assert card['round'] == 2 and [run['verdict'] for run in reviews] == ['FAIL', 'FAIL']
    assert all(run['report'] == 'VERDICT: FAIL\nmissing thing\n' for run in reviews)


def test_command_reviewer_prompt_stays_compatible(env):
    _, conn, _ = env
    card = api.get(conn, mk(conn, '[ok] command prompt', review=True))

    assert heads.reviewer_prompt(card, 1, 'worker evidence', {'exit_code': 0, 'output': 'accepted'}) == '\n'.join([
        'You are an independent REVIEWER. Do not redo the work; verify it against reality.',
        '# Card ' + card['id'] + ': [ok] command prompt', '', '## Goal', 'make out.txt', '',
        '## Acceptance (checked mechanically after you finish; exit 0 = pass)', '```', 'test -s out.txt', '```', '',
        '## Worker report (round 1)', 'worker evidence', '', '## Mechanical gate', 'exit 0: accepted', '',
        'Check the result (read files, run read-only commands). Then write REVIEW.md here whose',
        'FIRST line is exactly `VERDICT: PASS` or `VERDICT: FAIL`, followed by reasons.',
        'Review substance, not form: a FAIL costs the owner another round of waiting. Issue `VERDICT: FAIL` when',
        'the work itself falls short: the goal is not met, the result is wrong, a claim the owner will act on has',
        'no evidence, or work a criterion requires was not done (a change of code, configuration or docs, a test, a',
        'check that proves the change works, a merged PR, a part of the answer the goal asks for), or a rule of the',
        'task was broken.',
        'A FAIL lists at most 3 essential items, numbered. Each item names the criterion, the part of the goal or',
        'the rule it fails, then says what must change. The next worker reads them as its instructions, so nothing',
        'minor goes in them. With more than 3 essential items, name the 3 that matter most.',
        'Minor items go at the end of REVIEW.md under a heading `## Notes`, after a PASS or a FAIL alike. They go to',
        'the card author, not to the next worker, and they are never a reason for a FAIL.',
        'A record about the work (an issue, a ref, a link) is required work when the goal asks for it or the review',
        'fails anyway. Next to a finished and correct deliverable, a missing record is a note that names it.',
        'For a change (code, configuration, docs, a PR), every change, test and doc the criteria require is required',
        'work. For a question or a research, an answer that covers only part of what the goal asks about fails; but an',
        'item that widens the question is a note even when the criteria list it: other subjects next to the one the',
        'owner asked about, the CI of an already merged PR, extra statistics, extra context.',
        'A gap only in how the report presents finished work goes under `## Notes` after `VERDICT: PASS`: a list instead',
        'of a table, wording, length, order, a statement left implicit that the evidence shows, raw output next to a',
        'result with its source.',
        "The owner's clarifications outrank the criteria: when the owner asked for speed or a minimal change,",
        'everything beyond the deliverable they name (records, tables, statements) is a note.',
        'Before a FAIL, ask whether the owner would wait another round for the missing items. When the deliverable is',
        'done and correct and only notes are missing, the answer is no: `VERDICT: PASS` with the notes.',
        'Judge the result, not the artifacts: a PR, test or file the card names is the means of its goal. A worker',
        'who shows by a reproduction on the layer the user sees that the goal already holds needs no artifact;',
        'one who skipped it by reading code alone does. If the worker disputes the task itself (`RESULT: disputed`),',
        "say whether its evidence holds; the dispute goes to the card author, not to another round."])


def test_only_dispatcher_moves_and_cas(env):
    _, conn, _ = env
    cid = mk(conn, '[ok] cas')
    c = conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone()
    with store.tx(conn):
        assert store.cas_card(conn, c, 'queued')
    with store.tx(conn):
        assert not store.cas_card(conn, c, 'queued')  # stale version loses


# ── concurrency / crash / stale ─────────────────────────────────────────────

def test_concurrency_limit(env):
    cfg, conn, d = env
    ids = [mk(conn, f'[sleep] slow {i}') for i in range(4)]
    d.tick()
    running = conn.execute("SELECT count(*) FROM runs WHERE status IN ('starting','running')").fetchone()[0]
    assert running == cfg['concurrency'] == 2
    d.tick()
    assert conn.execute('SELECT count(*) FROM runs').fetchone()[0] == 2
    assert {r['state'] for r in api.listing(conn)} == {'running', 'queued'}
    for r in conn.execute('SELECT * FROM runs').fetchall():
        d.kill(r)
    del ids


def test_double_dispatch_no_duplicate_heads(env):
    cfg, conn, _ = env
    cid = mk(conn, '[sleep] contested')
    barrier = threading.Barrier(4)

    def go():
        x = dispatcher.Dispatcher(cfg, store.connect(cfg['db']), limits_fetcher=lambda c: {})
        barrier.wait()
        x.advance()
        x.launch()
    ts = [threading.Thread(target=go) for _ in range(4)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    assert conn.execute('SELECT count(*) FROM runs WHERE card_id=?', (cid,)).fetchone()[0] == 1
    assert conn.execute("SELECT count(*) FROM events WHERE card_id=? AND kind='claimed'", (cid,)).fetchone()[0] == 1
    d = dispatcher.Dispatcher(cfg, conn)
    time.sleep(1)
    for r in conn.execute('SELECT * FROM runs').fetchall():
        d.kill(r)


def test_tick_lock_skips_second(env):
    cfg, conn, d = env
    import fcntl
    with Path(cfg['db']).with_name('dispatcher.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        assert 'skipped' in d.tick()


def test_crashed_heads_wait_for_provider_recovery(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cid = mk(conn, '[crash] dies')
    end = time.time() + 60
    while time.time() < end:
        d.tick()
        c = api.get(conn, cid)
        if c['state'] == 'queued' and len(c['runs']) == 2:
            break
        time.sleep(0.2)
    else:
        raise AssertionError('provider failures did not reach outage wait')
    c = api.get(conn, cid)
    assert c['round'] == 1
    assert [r['status'] for r in c['runs']] == ['crashed', 'crashed']
    assert [r['provider_failure'] for r in c['runs']] == ['before_first_token'] * 2
    assert c['runs'][0]['vendor'] != c['runs'][1]['vendor'] and 'run_reaped' in kinds(conn, cid)
    assert 'provider_outage_wait' in kinds(conn, cid)


def test_one_shot_crash_fails_over_in_same_round(env):
    _, conn, d = env
    cid = mk(conn, '[ok] [crash-once] alternate after crash', step=2)
    assert run_until(d, conn, cid, timeout=60) == 'done'
    c = api.get(conn, cid)
    first, second = workers(c)
    assert c['round'] == 1 and [first['round'], second['round']] == [1, 1]
    assert [first['step'], second['step']] == [2, 2] and first['vendor'] != second['vendor']
    assert first['status'] == 'crashed' and first['provider_failure'] == 'before_first_token'
    assert second['status'] == 'exited' and second['provider_failure'] is None
    assert [e['data']['run'] for e in c['events'] if e['kind'] == 'provider_failover'] == [first['id']]


def test_stale_starting_claim_reaped(env):
    _, conn, d = env
    cid = mk(conn, '[ok] never launched')
    d.advance()
    d.spawn = lambda rid, wd: None  # launcher "succeeds" but nothing runs (lost spawn)
    d.launch()
    run = conn.execute('SELECT * FROM runs WHERE card_id=?', (cid,)).fetchone()
    assert run['status'] == 'starting'
    conn.execute('UPDATE runs SET started_at=? WHERE id=?', (time.time() - 100, run['id']))
    assert d.reap() == 1
    d.advance()
    c = api.get(conn, cid)
    assert c['state'] == 'queued' and c['runs'][0]['status'] == 'crashed'


def test_runner_duplicate_launch_loses(env):
    cfg, conn, d = env
    cid = mk(conn, '[ok] dup')
    d.advance()
    d.spawn = lambda rid, wd: None
    d.launch()
    rid = conn.execute('SELECT id FROM runs WHERE card_id=?', (cid,)).fetchone()[0]
    conn.execute("UPDATE runs SET status='running' WHERE id=?", (rid,))
    from hive import runner
    assert runner.main(['--db', cfg['db'], '--run-id', str(rid)]) == 3


# ── head pick / budget ──────────────────────────────────────────────────────

def test_weighted_pick_alternates():
    seq = []
    for _ in range(20):
        seq.append(heads.pick_vendor(['claude', 'codex'], seq[-12:], {'claude': 1, 'codex': 1}))
    assert max(len(s) for s in ''.join('a' if v == 'claude' else 'b' for v in seq).replace('ab', 'a b').replace('ba', 'b a').split()) <= 2
    assert abs(seq.count('claude') - seq.count('codex')) <= 1


def test_weighted_pick_respects_weights_without_long_streaks():
    seq = []
    for _ in range(30):
        seq.append(heads.pick_vendor(['claude', 'codex'], seq[-12:], {'claude': 2, 'codex': 1}))
    assert seq.count('claude') > seq.count('codex')
    s = ''.join('a' if v == 'claude' else 'b' for v in seq)
    assert 'aaa' not in s and 'bbb' not in s


def test_pick_single_allowed():
    assert heads.pick_vendor(['codex'], ['codex', 'codex', 'codex'], {}) == 'codex'
    assert heads.pick_vendor([], [], {}) is None


def test_worker_prefers_higher_known_weekly_quota_over_weight_and_history(env):
    cfg, conn, d = env
    cfg['vendors']['claude']['weight'] = 20
    d.limits_fetcher = lambda c: {'claude': 40.0, 'codex': 80.0}
    ids = [mk(conn, f'[ok] quota {i}') for i in range(2)]
    d.advance()
    d.launch()
    assert [workers(api.get(conn, cid))[0]['vendor'] for cid in ids] == ['codex', 'codex']


@pytest.mark.parametrize('limits', [{'claude': 80.0}, {'claude': 80.0, 'codex': 80.0}])
def test_worker_with_unknown_or_tied_weekly_quota_keeps_weighted_alternation(env, limits):
    _, conn, d = env
    d.limits_fetcher = lambda c: limits
    ids = [mk(conn, f'[ok] unknown quota {i}') for i in range(2)]
    d.advance()
    d.launch()
    assert [workers(api.get(conn, cid))[0]['vendor'] for cid in ids] == ['claude', 'codex']


def test_live_config_loads_and_is_complete():
    cfg = store.load_config(ROOT / 'config.example.toml')
    for key in ('db', 'workspaces', 'inbox', 'concurrency', 'max_rounds', 'held_long_s', 'budget'):
        assert key in cfg
    assert cfg['vendors']
    assert sorted(cfg['ladder']) == sorted(cfg['timeouts']) == [1, 2, 3, 4, 5]
    for s in range(1, 6):
        for v in cfg['vendors']:
            h = heads.head_for(cfg, s, v)
            assert h['model'] and h['effort']


def test_head_for_reads_the_configured_ladder(env):
    cfg, _, _ = env
    assert heads.head_for(cfg, 5, 'claude') == {'vendor': 'claude', 'model': 'claude-opus-5-5', 'effort': 'xhigh'}
    cfg['ladder'][5]['claude'] = {'model': 'configured-head', 'effort': 'configured-effort'}
    assert heads.head_for(cfg, 5, 'claude') == {
        'vendor': 'claude', 'model': 'configured-head', 'effort': 'configured-effort'}
    assert heads.head_for(cfg, 2, 'codex') == {'vendor': 'codex', 'model': 'gpt-6.1-terra', 'effort': 'medium'}


def test_budget_low_weekly_limits_steps(env):
    cfg, conn, _ = env
    ok, why = heads.allowed_vendors(conn, cfg, 3, {'claude': 15.0, 'codex': 90.0}, tag=None)
    assert ok == ['codex'] and 'weekly left' in why['claude']
    ok, _ = heads.allowed_vendors(conn, cfg, 2, {'claude': 15.0, 'codex': 90.0}, tag=None)
    assert ok == ['claude', 'codex']
    ok, _ = heads.allowed_vendors(conn, cfg, 5, {}, tag=None)  # unknown = allowed
    assert ok == ['claude', 'codex']


def spend_daily(conn, cfg, vendor):
    """Put `vendor` at its daily ceiling: a ceiling of 1 and one run started today."""
    cfg['vendors'][vendor]['daily_runs'] = 1
    cid = mk(conn, f'[ok] spent {vendor}')
    conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                 (cid, 1, 'worker', vendor, 'm', 'e', 1, 'p', 'exited', time.time()))
    conn.execute("UPDATE cards SET state='cancelled' WHERE id=?", (cid,))


def test_budget_daily_runs(env):
    cfg, conn, _ = env
    cfg['vendors']['claude']['daily_runs'] = 1
    cid = mk(conn, '[ok] x')
    conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                 (cid, 1, 'worker', 'claude', 'm', 'e', 1, 'p', 'exited', time.time()))
    ok, why = heads.allowed_vendors(conn, cfg, 1, {}, tag=None)
    assert ok == ['codex'] and 'daily' in why['claude']


@pytest.mark.parametrize('cap', ['absent', 0])
def test_daily_runs_is_an_optional_feature_off_by_default(env, cap):
    cfg, conn, _ = env
    if cap == 'absent':
        del cfg['vendors']['claude']['daily_runs']
    else:
        cfg['vendors']['claude']['daily_runs'] = cap
    cid = mk(conn, '[ok] x')
    for _ in range(200):
        conn.execute("INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at) VALUES(?,?,?,?,?,?,?,?,?,?)",
                     (cid, 1, 'worker', 'claude', 'm', 'e', 1, 'p', 'exited', time.time()))
    ok, why = heads.allowed_vendors(conn, cfg, 1, {}, tag=None)
    assert ok == ['claude', 'codex'] and why == {}
    cfg['vendors']['claude']['daily_runs'] = 200
    ok, why = heads.allowed_vendors(conn, cfg, 1, {}, tag=None)
    assert ok == ['codex'] and why['claude'] == 'daily runs limit 200 reached'


def test_budget_blocks_launch_waits(env):
    cfg, conn, d = env
    d.limits_fetcher = lambda c: {'claude': 5.0, 'codex': 5.0}
    cid = mk(conn, '[ok] big', step=4)
    d.tick()
    d.tick()
    assert api.get(conn, cid)['state'] == 'queued'
    assert kinds(conn, cid).count('budget_wait') == 1
    assert conn.execute("SELECT count(*) FROM outbox WHERE kind LIKE 'provider_%'").fetchone()[0] == 0


def test_a_launch_clears_the_budget_note(env):
    cfg, conn, d = env
    d.limits_fetcher = lambda c: {'claude': 5.0, 'codex': 5.0}
    cid = mk(conn, '[ok] big', step=4)
    d.tick()
    assert store.meta_get(conn, f'budget_wait:{cid}')[0] is not None
    d.limits_fetcher = lambda c: {'claude': 90.0, 'codex': 90.0}
    conn.execute("DELETE FROM meta WHERE key='limits'")
    d.tick()
    assert api.get(conn, cid)['runs']
    assert store.meta_get(conn, f'budget_wait:{cid}') == (None, None)


def test_daily_limit_and_task_failure_do_not_open_provider_episode(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    failed = mk(conn, '[exit1] task failure')
    assert tick_until_terminal(d, conn, failed) == 'blocked'
    for vendor in ('claude', 'codex'):
        spend_daily(conn, cfg, vendor)
    waiting = mk(conn, '[ok] daily limit')
    d.tick()
    assert api.get(conn, waiting)['state'] == 'queued'
    assert store.meta_get(conn, 'provider_outage_episode')[0] is None
    assert conn.execute("SELECT count(*) FROM outbox WHERE kind LIKE 'provider_%'").fetchone()[0] == 0


def test_parse_limits():
    data = {'providers': [{'provider': 'claude', 'windows': [{'window': '5h', 'left_pct': 90}, {'window': 'week', 'left_pct': 29.0}]},
                          {'provider': 'codex', 'windows': [{'window': 'week', 'left_pct': 95}]}]}
    assert heads.parse_limits(data) == {'claude': 29.0, 'codex': 95.0}
    assert heads.parse_limits(None) == {}


def test_verdict_parse():
    assert heads.parse_verdict('VERDICT: PASS\nok') == 'PASS'
    assert heads.parse_verdict('**VERDICT: FAIL**') == 'FAIL'
    assert heads.parse_verdict('looks fine') is None


def test_command_lines(env):
    cfg, _, _ = env
    a = heads.command(cfg, 'claude', 'claude-sonnet-5-5', 'medium', '/tmp', '/tmp/r', [])
    assert a[1:6] == ['-p', '--model', 'claude-sonnet-5-5', '--effort', 'medium']
    assert '--dangerously-skip-permissions' in a
    b = heads.command(cfg, 'codex', 'gpt-6-luna', 'medium', '/tmp/w', '/tmp/r', [])
    assert 'exec' in b and 'gpt-6-luna' in b and 'model_reasoning_effort="medium"' in b and b[-1] == '-'
    assert '--search' in b and '--dangerously-bypass-approvals-and-sandbox' in b
    assert b.index('--search') < b.index('--dangerously-bypass-approvals-and-sandbox') < b.index('exec')
    granted = {'HOME', 'PATH', 'LANG', 'USER', 'TERM', 'NO_COLOR', *cfg['head_env'],
               'SSL_CERT_FILE', 'SSL_CERT_DIR', 'XDG_RUNTIME_DIR', 'DBUS_SESSION_BUS_ADDRESS'}
    assert set(heads.head_env(cfg, 'claude', None)) <= granted


def test_tool_policy_reaches_vendor_commands_and_workspace_files(env):
    cfg, conn, d = env
    policy = ('browser', 'network', 'write')
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [provider429] restricted', deny_tools=policy)
    assert tick_until_terminal(d, conn, cid) == 'done'
    card = api.get(conn, cid)
    calls = [json.loads(line)['argv'] for line in Path(card['workdir'], 'calls.log').read_text().splitlines()]
    claude_args = next(args for args in calls if '-p' in args)
    assert '--dangerously-skip-permissions' in claude_args
    assert claude_args[claude_args.index('--disallowed-tools') + 1] == 'WebFetch,WebSearch,Bash,Edit,NotebookEdit,Write'
    assert claude_args[-1] == '--strict-mcp-config'

    codex_args = next(args for args in calls if 'exec' in args)
    assert '--search' not in codex_args and '--dangerously-bypass-approvals-and-sandbox' not in codex_args
    assert codex_args[codex_args.index('--sandbox') + 1] == 'read-only'
    assert codex_args[codex_args.index('--ask-for-approval') + 1] == 'never'
    assert 'sandbox_workspace_write.network_access=false' in codex_args
    assert codex_args.index('--ask-for-approval') < codex_args.index('exec') < codex_args.index('--sandbox')
    for name in ('CLAUDE.md', 'AGENTS.md'):
        assert 'browser access is denied.' in Path(card['workdir'], name).read_text()
        assert 'network access is denied.' in Path(card['workdir'], name).read_text()
        assert 'write access is denied.' in Path(card['workdir'], name).read_text()


def test_workspace_preparation_replaces_a_prior_tool_policy(env):
    cfg, _, _ = env
    card = {'id': 'policy-replacement', 'deny_tools': ['browser']}
    root = heads.prepare_workspace(cfg, card)
    initial = (root / 'CLAUDE.md').read_text()
    card['deny_tools'] = []
    heads.prepare_workspace(cfg, card)
    for name in ('CLAUDE.md', 'AGENTS.md'):
        text = (root / name).read_text()
        assert text != initial
        assert 'All default tools are available, including browser, network, and write access.' in text


def test_wait_returns_on_timeout(env):
    _, conn, _ = env
    cid = mk(conn, '[ok] w')
    t = time.time()
    assert api.wait(conn, cid, 0.3)['state'] == 'created' and time.time() - t < 2


def test_observe_flags(env):
    cfg, conn, d = env
    cid = mk(conn, '[never] obs')
    run_until(d, conn, cid)
    an = dispatcher.observe(conn, cfg)
    assert any(a['kind'] == 'blocked_24h' and a['card'] == cid for a in an)


def test_observe_flags_created_hold_beyond_default_limit(env, monkeypatch):
    cfg, conn, _ = env
    now = 1_000_000
    monkeypatch.setattr(api.time, 'time', lambda: now - cfg['held_long_s'] - 1)
    acceptance = 'x' * (dispatcher.EXCERPT + 1)
    cid, _ = api.create(conn, title='held beyond default', goal='make out.txt', acceptance=acceptance,
                        step=1, source='system:test', created_by='secretary', not_before=now + 1)

    held = [a for a in dispatcher.observe(conn, cfg, now=now) if a['kind'] == 'held_long']

    assert held == [{'kind': 'held_long', 'card': cid, 'age_s': cfg['held_long_s'] + 1,
                     'title': 'held beyond default', 'acceptance_tail': acceptance[-dispatcher.EXCERPT:]}]


def test_observe_held_long_uses_override_and_excludes_non_holds(env, monkeypatch):
    cfg, conn, _ = env
    now = 1_000_000
    cfg['held_long_s'] = 10
    monkeypatch.setattr(api.time, 'time', lambda: now - 10)
    boundary = mk(conn, 'at hold boundary', not_before=now + 1)
    monkeypatch.setattr(api.time, 'time', lambda: now - 11)
    held = mk(conn, 'beyond override', not_before=now + 1)
    scheduled = mk(conn, 'scheduled recheck', not_before=now + 1, recheck_every=1,
                   recheck_max=1)
    released = mk(conn, 'released hold', not_before=now + 1)
    api.release(conn, released, actor='test', at=now)
    queued = mk(conn, 'queued card', not_before=now + 1)
    conn.execute("UPDATE cards SET state='queued' WHERE id=?", (queued,))
    terminal = mk(conn, 'terminal card', not_before=now + 1)
    conn.execute("UPDATE cards SET state='done', finished_at=? WHERE id=?", (now, terminal))

    held_long = [a['card'] for a in dispatcher.observe(conn, cfg, now=now) if a['kind'] == 'held_long']

    assert held_long == [held]
    assert boundary not in held_long
    assert scheduled not in held_long
    assert released not in held_long
    assert queued not in held_long
    assert terminal not in held_long


def test_publish_retries_after_inbox_failure(env):
    _, conn, d = env

    class Broken:
        def packet(self, *a):
            return {}

        def publish(self, packet):
            raise OSError('inbox down')
    good = d.adapter().inbox
    d.adapter().inbox = Broken()
    cid = mk(conn, '[ok] pub')
    run_until(d, conn, cid)
    ob = api.get(conn, cid)['outbox'][0]
    assert ob['published_at'] is None and 'inbox down' in ob['error']
    d.adapter().inbox = good
    d.tick()
    assert api.get(conn, cid)['outbox'][0]['published_at']


def test_readonly_open_for_pulse(env):
    cfg, conn, d = env
    mk(conn, '[ok] ro')
    ro = sqlite3.connect(f"file:{cfg['db']}?mode=ro", uri=True)
    assert ro.execute('SELECT count(*) FROM cards').fetchone()[0] == 1
    with pytest.raises(sqlite3.OperationalError):
        ro.execute("UPDATE cards SET state='done'")


def test_hold_and_release(env):
    _, conn, d = env
    cid = mk(conn, '[ok] parked', not_before=time.time() + 3600)
    d.tick()
    assert api.get(conn, cid)['state'] == 'created'
    assert api.release(conn, cid)
    assert run_until(d, conn, cid) == 'done'
    assert 'released' in kinds(conn, cid)
    assert not api.release(conn, cid)  # only created cards



# ── blocked-by dependencies ─────────────────────────────────────────────────

def settle(conn, cid, state, reason=None):
    """Put a card straight into an outcome, standing in for the rounds that would reach it."""
    conn.execute('UPDATE cards SET state=?, block_reason=?, finished_at=?, version=version+1 WHERE id=?',
                 (state, reason, time.time(), cid))


def record_spawns(d):
    spawned = []
    d.spawn = lambda run_id, workdir: spawned.append(run_id)
    return spawned


def test_waiting_card_is_blocked_without_finishing_or_heads(env):
    cfg, conn, d = env
    spawned = record_spawns(d)
    blocker = mk(conn, '[ok] blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] dependent', blocked_by=[blocker])
    for _ in range(3):
        d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and card['round'] == 0 and card['runs'] == [] and spawned == []
    assert card['blocked_by'] == [{'id': blocker, 'state': 'created'}]
    assert card['block_reasons'] == [{'kind': 'dependency', 'blockers': [blocker]}]
    assert card['finished_at'] is None and card['outbox'] == [] and card['block_reason'] is None
    assert not any(a.get('card') == cid for a in dispatcher.observe(conn, cfg) if a['kind'] == 'blocked_24h')
    started = time.time()
    assert api.wait(conn, cid, 0.3, poll=0.05)['state'] == 'blocked'
    assert time.time() - started >= 0.3


def test_chain_runs_dependent_only_after_blocker_is_done(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    first = mk(conn, '[ok] first link')
    second = mk(conn, '[ok] second link', blocked_by=[first])
    for _ in range(10):
        d.tick()
        if store.is_final(card_row(conn, second)):
            break
    assert (api.get(conn, first)['state'], api.get(conn, second)['state']) == ('done', 'done')
    events = conn.execute("SELECT card_id, kind FROM events WHERE kind IN ('done','claimed','dependency_cleared') ORDER BY id").fetchall()
    order = [(e['card_id'], e['kind']) for e in events]
    assert order.index((first, 'done')) < order.index((second, 'dependency_cleared')) < order.index((second, 'claimed'))
    assert api.get(conn, second)['round'] == 1


def test_several_blockers_unblock_only_after_the_last(env):
    _, conn, d = env
    spawned = record_spawns(d)
    a, b = mk(conn, '[ok] a', not_before=time.time() + 3600), mk(conn, '[ok] b', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] needs both', blocked_by=[a, b])
    settle(conn, a, 'done')
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and card['block_reasons'] == [{'kind': 'dependency', 'blockers': [b]}]
    settle(conn, b, 'done')
    d.tick()
    k = kinds(conn, cid)
    assert k.count('dependency_cleared') == 1 and k.index('dependency_cleared') < k.index('queued')
    assert len(spawned) == 1


def test_failed_blocker_keeps_dependent_waiting(env):
    _, conn, d = env
    spawned = record_spawns(d)
    blocker = mk(conn, '[ok] failing blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] dependent of failure', blocked_by=[blocker])
    settle(conn, blocker, 'blocked', 'gate exit 1')
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and card['block_reasons'] == [{'kind': 'dependency', 'blockers': [blocker]}]
    settle(conn, blocker, 'done')
    d.tick()
    assert 'queued' in kinds(conn, cid) and len(spawned) == 1


def test_not_before_and_hold_still_apply_after_unblock(env, monkeypatch, capsys):
    cfg, conn, d = env
    spawned = record_spawns(d)
    now = [1000.0]
    d.clock = lambda: now[0]
    blocker = mk(conn, '[ok] early blocker', not_before=time.time() + 3600)
    later = mk(conn, '[ok] scheduled dependent', blocked_by=[blocker], not_before=2000.0)
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] held dependent', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'system:test', '--created-by', 'secretary', '--hold', '--blocked-by', blocker) == 0
    held = capsys.readouterr().out.split()[0]
    assert api.get(conn, held)['state'] == 'blocked'
    settle(conn, blocker, 'done')
    d.tick()
    for cid in (later, held):
        card = api.get(conn, cid)
        assert card['state'] == 'created' and card['block_reasons'] == [] and 'dependency_cleared' in kinds(conn, cid)
    assert spawned == []
    now[0] = 2001.0
    d.tick()
    assert 'queued' in kinds(conn, later) and 'queued' not in kinds(conn, held)
    assert api.release(conn, held)
    d.tick()
    assert 'queued' in kinds(conn, held)


def test_release_during_dependency_wait_lifts_the_hold(env):
    _, conn, d = env
    record_spawns(d)
    blocker = mk(conn, '[ok] blocker of held', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] held waiter', blocked_by=[blocker], not_before=time.time() + 3600)
    assert api.release(conn, cid)
    assert api.get(conn, cid)['state'] == 'blocked'
    settle(conn, blocker, 'done')
    d.tick()
    assert 'queued' in kinds(conn, cid)


def test_failure_reason_survives_dependency(env):
    _, conn, d = env
    record_spawns(d)
    blocker = mk(conn, '[ok] open blocker', not_before=time.time() + 3600)
    failed = mk(conn, '[ok] already failed', not_before=time.time() + 3600)
    settle(conn, failed, 'blocked', 'gate exit 1')
    assert api.add_blocker(conn, failed, blocker)
    assert api.get(conn, failed)['block_reasons'] == [{'kind': 'dependency', 'blockers': [blocker]},
                                                      {'kind': 'failure', 'reason': 'gate exit 1'}]
    settle(conn, blocker, 'done')
    d.tick()
    card = api.get(conn, failed)
    assert card['state'] == 'blocked' and card['block_reasons'] == [{'kind': 'failure', 'reason': 'gate exit 1'}]
    assert 'dependency_cleared' not in kinds(conn, failed)


def test_dependency_rejections_change_nothing(env):
    _, conn, _ = env
    a = mk(conn, '[ok] ra', not_before=time.time() + 3600)
    b = mk(conn, '[ok] rb', blocked_by=[a])
    c = mk(conn, '[ok] rc', blocked_by=[b])
    finished = mk(conn, '[ok] finished')
    settle(conn, finished, 'done')
    before = conn.execute('SELECT count(*) FROM events').fetchone()[0]
    for card_id, blocker_id in ((a, a), (a, c), (a, b), (a, 'h_missing'), ('h_missing', a), (finished, a)):
        with pytest.raises(api.CardError):
            api.add_blocker(conn, card_id, blocker_id)
    for card_id, blocker_id in ((b, 'h_missing'), ('h_missing', a)):
        with pytest.raises(api.CardError):
            api.remove_blocker(conn, card_id, blocker_id)
    for blocked_by in (['h_missing'], a):
        with pytest.raises(api.CardError):
            mk(conn, '[ok] bad create', blocked_by=blocked_by)
    with pytest.raises(api.CardError):
        mk(conn, '[ok] self create', card_id='h_self', blocked_by=['h_self'])
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == before
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 4
    assert api.get(conn, a)['blocked_by'] == [] and api.get(conn, a)['state'] == 'created'


def test_queued_card_waits_when_a_dependency_is_added(env):
    _, conn, d = env
    spawned = record_spawns(d)
    d.cfg = dict(d.cfg, concurrency=0)
    blocker = mk(conn, '[ok] late blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] queued dependent')
    d.tick()
    assert api.get(conn, cid)['state'] == 'queued'
    assert api.add_blocker(conn, cid, blocker)
    d.tick()
    assert api.get(conn, cid)['state'] == 'blocked' and spawned == []


def test_dependency_add_and_remove_are_idempotent(env):
    _, conn, d = env
    record_spawns(d)
    blocker = mk(conn, '[ok] idem blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] idem dependent', not_before=time.time() + 3600)
    assert api.add_blocker(conn, cid, blocker) and not api.add_blocker(conn, cid, blocker)
    assert kinds(conn, cid).count('dependency_added') == 1 and kinds(conn, cid).count('dependency_wait') == 1
    assert api.remove_blocker(conn, cid, blocker) and not api.remove_blocker(conn, cid, blocker)
    assert kinds(conn, cid).count('dependency_removed') == 1
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'created' and card['blocked_by'] == [] and kinds(conn, cid).count('dependency_cleared') == 1


def test_dependency_survives_new_connection_and_dispatcher(env):
    cfg, conn, _ = env
    blocker = mk(conn, '[ok] durable blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] durable dependent', not_before=time.time() + 3600)
    api.add_blocker(conn, cid, blocker)
    conn.close()
    fresh = store.connect(cfg['db'])
    d = dispatcher.Dispatcher(cfg, fresh, limits_fetcher=lambda c: {})
    record_spawns(d)
    d.advance()
    assert api.get(fresh, cid)['blocked_by'] == [{'id': blocker, 'state': 'created'}]
    assert api.get(fresh, cid)['state'] == 'blocked'
    settle(fresh, blocker, 'done')
    d.advance()
    assert api.get(fresh, cid)['state'] == 'created'


def test_dependency_cli(env, monkeypatch, capsys):
    cfg, conn, _ = env
    a = mk(conn, '[ok] cli a', not_before=time.time() + 3600)
    b = mk(conn, '[ok] cli b', not_before=time.time() + 3600)
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] cli dependent', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'system:test', '--created-by', 'secretary', '--blocked-by', a, '--blocked-by', b) == 0
    cid, _, state = capsys.readouterr().out.split()
    assert state == 'state=blocked'
    assert [x['id'] for x in api.get(conn, cid)['blocked_by']] == [a, b]
    assert hive_cli(monkeypatch, cfg, 'undepend', cid, a) == 0
    assert 'changed' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'undepend', cid, a) == 0
    assert 'unchanged' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'depend', a, cid) == 0
    capsys.readouterr()
    for argv in (('depend', b, cid), ('depend', cid, cid), ('depend', cid, 'h_missing'), ('undepend', 'h_missing', a)):
        assert hive_cli(monkeypatch, cfg, *argv) == 2
        assert 'hive: dependency rejected' in capsys.readouterr().err
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    out = capsys.readouterr().out
    assert f'blocked by: {b} [created]' in out and '"kind": "dependency"' in out


# ── epics ───────────────────────────────────────────────────────────────────

HELD = {'not_before': time.time() + 10 * 86400}


def mk_epic(conn, title, acceptance='true', **kw):
    cid, _ = api.create(conn, title=title, goal='ship the parts', acceptance=acceptance, step=1,
                        source='system:test', created_by='secretary', kind='epic', **kw)
    return cid


def counts(conn):
    return tuple(conn.execute(f'SELECT count(*) FROM {t}').fetchone()[0] for t in ('cards', 'links', 'events', 'outbox'))


def test_epic_finishes_only_after_last_required_child_and_its_gate(env):
    _, conn, d = env
    spawned = record_spawns(d)
    epic = mk_epic(conn, '[ok] epic')
    a = mk(conn, '[ok] part a', parent=epic, **HELD)
    b = mk(conn, '[ok] part b', parent=epic, **HELD)
    extra = mk(conn, '[ok] nice to have', parent=epic, optional=True, **HELD)
    dependent = mk(conn, '[ok] after the epic', blocked_by=[epic], **HELD)
    d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'blocked' and card['wait_reason'] == 'children'
    assert card['block_reasons'] == [{'kind': 'children', 'pending': [a, b]}]
    assert card['progress'] == {'done': 0, 'total': 2, 'blocked_waiting': 0, 'blocked_failed': 0}
    assert card['finished_at'] is None and card['outbox'] == []
    assert api.wait(conn, epic, 0.2, poll=0.05)['state'] == 'blocked'
    settle(conn, a, 'done')
    d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'blocked' and card['progress']['done'] == 1 and card['gates'] == []
    settle(conn, b, 'done')
    d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'done' and card['round'] == 0 and card['runs'] == [] and spawned == []
    assert [g['passed'] for g in card['gates']] == [1]
    assert [o['kind'] for o in card['outbox']] == ['task_result']
    assert card['progress'] == {'done': 2, 'total': 2, 'blocked_waiting': 0, 'blocked_failed': 0}
    k = kinds(conn, epic)
    assert 'queued' not in k and 'claimed' not in k and k.index('children_done') < k.index('done')
    assert api.get(conn, extra)['state'] == 'created'
    d.tick()
    assert api.get(conn, dependent)['state'] == 'created' and 'dependency_cleared' in kinds(conn, dependent)


def test_epic_failed_acceptance_blocks_without_retry(env):
    _, conn, d = env
    spawned = record_spawns(d)
    epic = mk_epic(conn, '[ok] failing epic', acceptance='false')
    child = mk(conn, '[ok] only part', parent=epic, **HELD)
    settle(conn, child, 'done')
    for _ in range(3):
        d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'blocked' and store.is_final(card) and card['block_reason'] == 'gate exit 1'
    assert card['block_reasons'] == [{'kind': 'failure', 'reason': 'gate exit 1'}]
    assert [o['kind'] for o in card['outbox']] == ['task_blocked'] and len(card['gates']) == 1
    assert card['round'] == 0 and card['runs'] == [] and spawned == [] and 'retry' not in kinds(conn, epic)


def test_epic_without_required_children_keeps_waiting(env):
    _, conn, d = env
    spawned = record_spawns(d)
    epic = mk_epic(conn, '[ok] empty epic')
    d.tick()
    assert api.get(conn, epic)['progress'] == {'done': 0, 'total': 0, 'blocked_waiting': 0, 'blocked_failed': 0}
    only_optional = mk(conn, '[ok] optional only', parent=epic, optional=True, **HELD)
    settle(conn, only_optional, 'done')
    required = mk(conn, '[ok] detached later', parent=epic, **HELD)
    settle(conn, required, 'done')
    assert api.remove_child(conn, required, epic)
    for _ in range(3):
        d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'blocked' and card['wait_reason'] == 'children' and card['gates'] == []
    assert card['children'] == [{'id': only_optional, 'state': 'done', 'optional': True}]
    assert card['round'] == 0 and card['runs'] == [] and spawned == []


def test_epic_progress_separates_waiting_and_failed_children(env, monkeypatch, capsys):
    cfg, conn, d = env
    record_spawns(d)
    epic = mk_epic(conn, '[ok] tracked epic')
    held = mk(conn, '[ok] upstream', **HELD)
    done = mk(conn, '[ok] finished part', parent=epic, **HELD)
    failed = mk(conn, '[ok] failed part', parent=epic, **HELD)
    waiting = mk(conn, '[ok] waiting part', parent=epic, blocked_by=[held])
    optional = mk(conn, '[ok] failed optional', parent=epic, optional=True, **HELD)
    settle(conn, done, 'done')
    settle(conn, failed, 'blocked', 'gate exit 1')
    settle(conn, optional, 'blocked', 'gate exit 1')
    for _ in range(3):
        d.tick()
    card = api.get(conn, epic)
    assert card['state'] == 'blocked' and not store.is_final(card)
    assert card['progress'] == {'done': 1, 'total': 3, 'blocked_waiting': 1, 'blocked_failed': 1}
    assert card['children'] == [{'id': done, 'state': 'done', 'optional': False},
                                {'id': failed, 'state': 'blocked', 'optional': False},
                                {'id': waiting, 'state': 'blocked', 'optional': False},
                                {'id': optional, 'state': 'blocked', 'optional': True}]
    assert card['block_reasons'] == [{'kind': 'children', 'pending': [failed, waiting]}]
    assert api.get(conn, waiting)['parent'] == {'id': epic, 'state': 'blocked', 'optional': False}
    assert api.get(conn, held)['parent'] is None and api.get(conn, held)['progress'] is None
    rows = {r['id']: r['parent'] for r in api.listing(conn)}
    assert rows[waiting] == epic and rows[held] is None and rows[epic] is None
    assert hive_cli(monkeypatch, cfg, 'show', epic) == 0
    out = capsys.readouterr().out
    assert 'progress: 1/3 required done; blocked: 1 waiting, 1 failed' in out
    assert f'child {optional} [blocked] optional' in out and '"kind": "children"' in out
    assert hive_cli(monkeypatch, cfg, 'show', optional) == 0
    assert f'epic: {epic} [blocked] optional' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'list') == 0
    assert f'(epic {epic})' in capsys.readouterr().out


def test_epic_link_rejections_change_nothing(env):
    _, conn, _ = env
    outer = mk_epic(conn, '[ok] outer')
    inner = mk_epic(conn, '[ok] inner', parent=outer)
    other = mk_epic(conn, '[ok] other')
    task = mk(conn, '[ok] task', parent=inner, **HELD)
    loose = mk(conn, '[ok] loose', **HELD)
    finished = mk_epic(conn, '[ok] finished epic')
    settle(conn, finished, 'done')
    before = counts(conn)
    for child, epic in ((loose, task), (loose, 'h_missing'), ('h_missing', other), (other, other),
                        (task, other), (outer, inner), (outer, task), (loose, finished)):
        with pytest.raises(api.CardError):
            api.add_child(conn, child, epic)
    for child, epic in ((task, 'h_missing'), ('h_missing', inner)):
        with pytest.raises(api.CardError):
            api.remove_child(conn, child, epic)
    for kw in ({'parent': task}, {'parent': 'h_missing'}, {'parent': finished}, {'optional': True}):
        with pytest.raises(api.CardError):
            mk(conn, '[ok] bad child', **kw)
    for kw in ({'acceptance_kind': 'judge', 'judge_criteria': 'c'}, {'blocked_by': [loose]}, {'review': True},
               {'not_before': time.time() + 60}, {'recheck_every': 60, 'recheck_max': 1}):
        with pytest.raises(api.CardError):
            mk_epic(conn, '[ok] bad epic', **kw)
    with pytest.raises(api.CardError):
        api.add_blocker(conn, other, loose)
    assert counts(conn) == before
    assert api.get(conn, loose)['parent'] is None and api.get(conn, task)['parent']['id'] == inner


def test_epic_links_are_idempotent_and_journaled_on_both_sides(env):
    _, conn, _ = env
    epic = mk_epic(conn, '[ok] journaled epic')
    child = mk(conn, '[ok] journaled child', **HELD)
    assert api.add_child(conn, child, epic) and not api.add_child(conn, child, epic)
    assert api.add_child(conn, child, epic, optional=True) and not api.add_child(conn, child, epic, optional=True)
    assert api.remove_child(conn, child, epic) and not api.remove_child(conn, child, epic)
    for cid in (child, epic):
        k = kinds(conn, cid)
        assert (k.count('child_added'), k.count('child_changed'), k.count('child_removed')) == (1, 1, 1)
    assert api.get(conn, child)['parent'] is None and api.get(conn, epic)['children'] == []


def test_epic_cli(env, monkeypatch, capsys):
    cfg, conn, _ = env
    base = ('--goal', 'g', '--acceptance', 'true', '--step', '1', '--source', 'system:test', '--created-by', 'secretary')
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] cli epic', '--epic', *base) == 0
    epic, _, state = capsys.readouterr().out.split()
    assert state == 'state=blocked' and api.get(conn, epic)['kind'] == 'epic'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] cli child', '--parent', epic, '--optional', '--hold', *base) == 0
    child = capsys.readouterr().out.split()[0]
    assert api.get(conn, child)['parent'] == {'id': epic, 'state': 'blocked', 'optional': True}
    assert hive_cli(monkeypatch, cfg, 'attach', child, epic) == 0
    assert 'changed' in capsys.readouterr().out and not api.get(conn, child)['parent']['optional']
    assert hive_cli(monkeypatch, cfg, 'detach', child, epic) == 0
    assert hive_cli(monkeypatch, cfg, 'detach', child, epic) == 0
    assert 'unchanged' in capsys.readouterr().out.splitlines()[-1]
    before = counts(conn)
    for argv in (('attach', epic, epic), ('attach', child, 'h_missing'), ('attach', epic, child)):
        assert hive_cli(monkeypatch, cfg, *argv) == 2
        assert 'hive: epic link rejected' in capsys.readouterr().err
    for argv in (('--title', 'x', '--epic', '--blocked-by', child), ('--title', 'x', '--parent', child),
                 ('--title', 'x', '--optional')):
        assert hive_cli(monkeypatch, cfg, 'create', *argv, *base) == 2
        assert 'hive: card rejected' in capsys.readouterr().err
    assert counts(conn) == before


def test_detach_from_non_epic_rejects_and_from_unlinked_epic_is_unchanged(env, monkeypatch, capsys):
    cfg, conn, _ = env
    epic = mk_epic(conn, '[ok] detach epic')
    task = mk(conn, '[ok] detach target task', **HELD)
    child = mk(conn, '[ok] detach child', parent=epic, **HELD)
    before = counts(conn)
    with pytest.raises(api.CardError, match='not an epic'):
        api.remove_child(conn, child, task)
    assert hive_cli(monkeypatch, cfg, 'detach', child, task) == 2
    assert 'hive: epic link rejected' in capsys.readouterr().err
    assert counts(conn) == before and api.get(conn, child)['parent']['id'] == epic
    other = mk_epic(conn, '[ok] unrelated epic')
    before = counts(conn)
    assert not api.remove_child(conn, child, other)
    assert hive_cli(monkeypatch, cfg, 'detach', child, other) == 0
    assert 'unchanged' in capsys.readouterr().out
    assert counts(conn) == before


def test_release_of_epic_rejects_without_event(env, monkeypatch, capsys):
    cfg, conn, _ = env
    epic = mk_epic(conn, '[ok] release epic')
    before = counts(conn)
    with pytest.raises(api.CardError):
        api.release(conn, epic)
    assert hive_cli(monkeypatch, cfg, 'release', epic) == 2
    assert 'hive: release rejected' in capsys.readouterr().err
    assert counts(conn) == before and api.get(conn, epic)['not_before'] is None


def test_steer_of_epic_rejects_without_event(env, monkeypatch, capsys):
    cfg, conn, _ = env
    epic = mk_epic(conn, '[ok] steer epic')
    before = counts(conn)
    with pytest.raises(api.CardError):
        api.steer(conn, epic, 'use the other API', actor='owner')
    assert hive_cli(monkeypatch, cfg, 'steer', epic, 'use the other API') == 2
    assert 'hive: steer rejected' in capsys.readouterr().err
    assert counts(conn) == before and api.get(conn, epic)['steers'] == []


def test_legacy_links_without_parents_open_unchanged(tmp_path):
    db = tmp_path / 'legacy.db'
    current = store.connect(tmp_path / 'scratch.db')
    cards_sql = current.execute("SELECT sql FROM sqlite_master WHERE name='cards'").fetchone()[0]
    current.close()
    conn = sqlite3.connect(db)
    conn.execute(cards_sql)
    conn.execute("""CREATE TABLE links(card_id TEXT NOT NULL REFERENCES cards(id), kind TEXT NOT NULL,
                    target_id TEXT NOT NULL REFERENCES cards(id), created_at REAL NOT NULL,
                    PRIMARY KEY(card_id, kind, target_id))""")
    for cid, state, wait in (('old_done', 'done', None), ('old_wait', 'blocked', 'dependency'), ('old_new', 'created', None)):
        conn.execute("""INSERT INTO cards(id,title,goal,acceptance,step,source,created_by,state,wait_reason,created_at,updated_at)
                        VALUES(?,?,'g','true',1,'s','secretary',?,?,1,1)""", (cid, cid, state, wait))
    conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES('old_wait','blocked_by','old_new',1)")
    conn.commit()
    conn.close()
    migrated = store.connect(db, owner_actor='owner')
    for cid, state in (('old_done', 'done'), ('old_wait', 'blocked'), ('old_new', 'created')):
        card = api.get(migrated, cid)
        assert (card['kind'], card['state'], card['parent'], card['children'], card['progress']) == ('task', state, None, [], None)
    assert api.get(migrated, 'old_wait')['blocked_by'] == [{'id': 'old_new', 'state': 'created'}]
    assert {r['parent'] for r in api.listing(migrated)} == {None}
    epic = mk_epic(migrated, '[ok] new epic')
    assert api.add_child(migrated, 'old_new', epic, optional=True)
    assert api.get(store.connect(db, owner_actor='owner'), 'old_new')['parent'] == {'id': epic, 'state': 'blocked', 'optional': True}


# ── steer ───────────────────────────────────────────────────────────────────

def hive_cli(monkeypatch, cfg, *argv):
    import importlib.util
    spec = importlib.util.spec_from_file_location('hive_cli', ROOT / 'scripts' / 'hive.py')
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    return mod.main(list(argv))


def card_row(conn, cid):
    return dict(conn.execute('SELECT * FROM cards WHERE id=?', (cid,)).fetchone())


def test_create_cli_judge_acceptance_uses_report_gate_without_review(env, monkeypatch, capsys):
    """--accept-review is the judge's acceptance only; it turns on no reviewer (owner, 26.11)."""
    cfg, conn, _ = env
    criteria = 'The report explains the result and its evidence.'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'judge card', '--goal', 'g',
                    '--accept-review', criteria, '--step', '1', '--source', 'manual', '--kind', 'system',
                    '--created-by', 'secretary') == 0
    cid = capsys.readouterr().out.split()[0]
    card = api.get(conn, cid)
    assert (card['acceptance_kind'], card['judge_criteria'], card['acceptance'], card['review']) == (
        'judge', criteria, 'test -s REPORT.md', 0)


def test_create_cli_judge_acceptance_keeps_explicit_review(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'judge card', '--goal', 'g',
                    '--accept-review', 'c', '--review', '--step', '1', '--source', 'manual', '--kind', 'system',
                    '--created-by', 'secretary') == 0
    cid = capsys.readouterr().out.split()[0]
    assert (api.get(conn, cid)['acceptance_kind'], api.get(conn, cid)['review']) == ('judge', 1)


def test_judge_card_without_review_is_done_by_the_judge_alone(env, monkeypatch, capsys):
    cfg, conn, d = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] judge only', '--goal', 'make out.txt',
                    '--accept-review', 'The result explains the outcome.', '--step', '1', '--source', 'manual',
                    '--kind', 'system', '--created-by', 'secretary') == 0
    cid = capsys.readouterr().out.split()[0]
    run_heads_inline(cfg, d)

    assert tick_until_terminal(d, conn, cid) == 'done'
    assert [run['role'] for run in api.get(conn, cid)['runs']] == ['worker']


def test_create_cli_command_acceptance_keeps_explicit_review(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'command card', '--goal', 'g',
                    '--acceptance', 'test -s output.txt', '--review', '--step', '1',
                    '--source', 'manual', '--kind', 'system', '--created-by', 'secretary') == 0
    cid = capsys.readouterr().out.split()[0]
    card = api.get(conn, cid)
    assert (card['acceptance_kind'], card['judge_criteria'], card['acceptance'], card['review']) == (
        'command', None, 'test -s output.txt', 1)
    assert card['deny_tools'] == []


def test_create_cli_forwards_recheck_schedule_and_timestamp(env, monkeypatch, capsys):
    cfg, conn, _ = env
    monkeypatch.setattr(api.time, 'time', lambda: 1700000000)
    not_before = '2030-01-02T03:04:05+02:00'
    until = '2030-01-04T03:04:05+02:00'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'scheduled card', '--goal', 'g',
                    '--acceptance', 'true', '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary',
                    '--not-before', not_before, '--recheck-every', '15m', '--recheck-until', until,
                    '--recheck-max', '2') == 0
    cid = capsys.readouterr().out.split()[0]
    card = api.get(conn, cid)
    assert card['not_before'] == dt.datetime(2030, 1, 2, 1, 4, 5, tzinfo=dt.timezone.utc).timestamp()
    until_ts = dt.datetime(2030, 1, 4, 1, 4, 5, tzinfo=dt.timezone.utc).timestamp()
    assert card['recheck_policy'] == {'every': 900.0, 'until': until_ts, 'max': 2}
    assert card['recheck_state'] == {'due': 1700000900, 'attempts': 0}


def test_create_cli_hold_and_schedule_rejections_do_not_create_cards(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'held card', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary', '--hold') == 0
    held = api.get(conn, capsys.readouterr().out.split()[0])
    assert held['not_before'] > time.time() and held['recheck_policy'] is None
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    bad_args = [
        ('--hold', '--not-before', '2030-01-01T00:00:00+00:00'),
        ('--not-before', '2030-01-01T00:00:00'),
        ('--recheck-every', '0s', '--recheck-max', '1'),
    ]
    for extra in bad_args:
        with pytest.raises(SystemExit) as exc:
            hive_cli(monkeypatch, cfg, 'create', '--title', 'invalid card', '--goal', 'g', '--acceptance', 'true',
                     '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary', *extra)
        assert exc.value.code == 2
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'invalid card', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary',
                    '--recheck-every', '1h', '--recheck-max', '0') == 2
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def test_create_cli_tool_denials_and_rejection(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'restricted card', '--goal', 'g',
                    '--acceptance', 'true', '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary',
                    '--deny-tools', 'browser,network,write') == 0
    cid = capsys.readouterr().out.split()[0]
    assert api.get(conn, cid)['deny_tools'] == ['browser', 'network', 'write']
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(SystemExit) as exc:
        hive_cli(monkeypatch, cfg, 'create', '--title', 'invalid card', '--goal', 'g',
                 '--acceptance', 'true', '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary',
                 '--deny-tools', 'browser,browser')
    assert exc.value.code == 2
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


@pytest.mark.parametrize('acceptance_args', [(), ('--acceptance', 'true', '--accept-review', 'criteria')])
def test_create_cli_requires_one_acceptance_form(env, monkeypatch, capsys, acceptance_args):
    cfg, conn, _ = env
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(SystemExit) as exc:
        hive_cli(monkeypatch, cfg, 'create', '--title', 'invalid card', '--goal', 'g',
                 '--step', '1', '--source', 'manual', '--kind', 'system', '--created-by', 'secretary', *acceptance_args)
    assert exc.value.code == 2
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before
    error = capsys.readouterr().err
    if acceptance_args:
        assert 'not allowed with argument' in error
    else:
        assert 'one of the arguments --acceptance --accept-review is required' in error


def test_steer_records_verbatim_event_and_keeps_card(env):
    _, conn, d = env
    cid = mk(conn, '[ok] steered', body='old body')
    assert run_until(d, conn, cid) == 'done'
    before = card_row(conn, cid)
    api.steer(conn, cid, '  use the v2 endpoint\n', actor='owner')
    assert card_row(conn, cid) == before
    ev = api.get(conn, cid)['events'][-1]
    assert ev['kind'] == 'steer' and ev['actor'] == 'owner' and ev['data'] == {'text': '  use the v2 endpoint\n', 'by': 'owner'}


def test_steer_rejects_without_event(env):
    _, conn, _ = env
    cid = mk(conn, '[ok] strict')
    before = conn.execute('SELECT count(*) FROM events').fetchone()[0]
    for card, text in ((cid, ''), (cid, '  \n\t'), (cid, None), ('h_missing', 'real text')):
        with pytest.raises(api.CardError):
            api.steer(conn, card, text, actor='owner')
    with pytest.raises(api.CardError):
        api.steer(conn, cid, 'real text', actor=' ')
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == before
    assert 'steer' not in kinds(conn, cid)


def test_steer_cli_records_and_show_reads(env, monkeypatch, capsys):
    cfg, conn, _ = env
    cid = mk(conn, '[ok] cli steer', not_before=time.time() + 3600)
    before = card_row(conn, cid)
    assert hive_cli(monkeypatch, cfg, 'steer', cid, 'скорее, пожалуйста', '--by', 'owner') == 0
    out = capsys.readouterr().out
    assert 'recorded' in out and 'not delivered' in out
    assert card_row(conn, cid) == before
    ev = api.get(conn, cid)['events'][-1]
    assert (ev['kind'], ev['actor'], ev['data']) == ('steer', 'owner', {'text': 'скорее, пожалуйста', 'by': 'owner'})
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    assert 'steer {"text": "скорее, пожалуйста", "by": "owner"}' in capsys.readouterr().out


def test_steer_cli_rejects_without_event(env, monkeypatch, capsys):
    cfg, conn, _ = env
    cid = mk(conn, '[ok] cli strict')
    before = conn.execute('SELECT count(*) FROM events').fetchone()[0]
    assert hive_cli(monkeypatch, cfg, 'steer', cid, '   ') == 2
    assert 'steer rejected: steer text is required' in capsys.readouterr().err
    assert hive_cli(monkeypatch, cfg, 'steer', 'h_missing', 'text') == 2
    assert "steer rejected: unknown card 'h_missing'" in capsys.readouterr().err
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == before


# ── late steer: a clarification after the worker report ───────────────────────

def run_heads_inline(cfg, d):
    """Each head launch runs the real runner and fake head to completion before launch() returns."""
    def spawn(run_id, workdir):
        subprocess.run([sys.executable, '-m', 'hive.runner', '--db', cfg['db'], '--run-id', str(run_id)], cwd=str(workdir),
                       env=dict(os.environ, PYTHONPATH=str(ROOT)), stdin=subprocess.DEVNULL, check=True)
    d.spawn = spawn


def tick_until_terminal(d, conn, cid, ticks=10):
    for _ in range(ticks):
        d.tick()
        st = conn.execute('SELECT state FROM cards WHERE id=?', (cid,)).fetchone()['state']
        if st in store.TERMINAL:
            return st
    raise AssertionError(f'card {cid} not terminal after {ticks} ticks: {st}')


def workers(c):
    return [r for r in c['runs'] if r['role'] == 'worker']


def claimed_steers(conn, cid):
    return [e['data'].get('steers') for e in api.get(conn, cid)['events']
            if e['kind'] == 'claimed' and e['data']['role'] == 'worker']


def test_steer_after_report_before_gate_runs_next_round(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] late steer gate')
    d.advance()
    d.launch()
    assert api.get(conn, cid)['runs'][0]['report'].startswith('round 1')
    api.steer(conn, cid, 'use the v2 endpoint\n  keep `42`', actor='owner')
    d.advance()
    c = api.get(conn, cid)
    assert (c['state'], c['outbox'], [g['passed'] for g in c['gates']]) == ('queued', [], [1])
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    prompt = workers(c)[1]['prompt']
    assert c['round'] == 2 and len(workers(c)) == 2
    assert '### Arrived after the round 1 report (from owner)\nuse the v2 endpoint\n  keep `42`' in prompt
    assert 'Previous round failed' not in prompt
    assert [o['kind'] for o in c['outbox']] == ['task_result']
    payload = json.loads(c['outbox'][0]['payload'])
    assert payload['rounds'] == 2 and payload['report_excerpt'].startswith('round 2')
    assert kinds(conn, cid).count('steer_round') == 1 and kinds(conn, cid).count('done') == 1


def test_steer_before_final_review_runs_next_round(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] late steer review', review=True)
    d.advance()
    d.launch()
    d.advance()
    assert api.get(conn, cid)['state'] == 'review'
    api.steer(conn, cid, 'also cover the empty case', actor='owner')
    d.launch()
    d.advance()
    c = api.get(conn, cid)
    assert c['state'] == 'queued' and c['outbox'] == [] and c['runs'][-1]['verdict'] == 'PASS'
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [r['verdict'] for r in c['runs'] if r['role'] == 'reviewer'] == ['PASS', 'PASS']
    assert '### Arrived after the round 1 report (from owner)\nalso cover the empty case' in workers(c)[1]['prompt']
    assert [o['kind'] for o in c['outbox']] == ['task_result']
    assert json.loads(c['outbox'][0]['payload'])['reason'] == 'judge done'


def test_several_late_steers_one_round_none_dropped(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] two steers')
    other = mk(conn, '[ok] bystander')
    d.advance()
    d.launch()
    api.steer(conn, cid, 'first: plain text', actor='owner')
    api.steer(conn, cid, 'second: with a table', actor='secretary')
    d.advance()
    assert api.get(conn, cid)['state'] == 'queued'
    api.steer(conn, cid, 'third: while queued', actor='owner')
    d.advance()
    assert tick_until_terminal(d, conn, cid) == 'done' and tick_until_terminal(d, conn, other) == 'done'
    for _ in range(3):
        d.tick()
    c = api.get(conn, cid)
    steer_ids = [e['id'] for e in c['events'] if e['kind'] == 'steer']
    assert len(workers(c)) == 2 and claimed_steers(conn, cid) == [None, steer_ids]
    assert [e['data']['steers'] for e in c['events'] if e['kind'] == 'steer_round'] == [steer_ids[:2]]
    prompt = workers(c)[1]['prompt']
    assert prompt.index('first: plain text') < prompt.index('second: with a table') < prompt.index('third: while queued')
    assert '### Arrived after the round 1 report (from secretary)\nsecond: with a table' in prompt
    assert len(c['outbox']) == 1
    assert api.get(conn, other)['round'] == 1 and 'steer_round' not in kinds(conn, other)


def test_late_steer_with_single_failure_budget_finishes(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] late steer with one failure allowed')
    d.advance()
    d.launch()
    api.steer(conn, cid, 'switch to csv', actor='owner')
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert len(workers(c)) == 2 and deliveries(conn, cid) == [('acknowledged', workers(c)[1]['id'])]
    assert [o['kind'] for o in c['outbox']] == ['task_result']


def test_steer_racing_gate_follows_transaction_order(env, tmp_path):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    script = tmp_path / 'steer_now.py'
    script.write_text(f"import sys\nsys.path.insert(0, {str(ROOT)!r})\nfrom hive import api, store\n"
                      f"api.steer(store.connect({cfg['db']!r}), sys.argv[1], 'steer during the gate', actor='owner')\n")
    cid, _ = api.create(conn, title='[ok] racing gate', goal='make out.txt', step=1, source='system:test', created_by='secretary',
                        acceptance=f'test -s out.txt && {{ test -e steered || {{ touch steered && {sys.executable} {script} "$HIVE_CARD"; }}; }}')
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [g['passed'] for g in c['gates']] == [1, 1]
    assert 'steer during the gate' in workers(c)[1]['prompt'] and kinds(conn, cid).count('steer_round') == 1
    api.steer(conn, cid, 'too late', actor='owner')
    d.tick()
    d.tick()
    after = api.get(conn, cid)
    assert (after['state'], len(after['runs']), len(after['outbox'])) == ('done', 2, 1)
    assert kinds(conn, cid).count('steer_round') == 1 and len(claimed_steers(conn, cid)) == 2


# ── active steer: a clarification while the worker head runs ──────────────────

class Live:
    """Heads launched by the dispatcher run in real runner processes; a `hold-r<n>` FIFO keeps that worker round active.
    Every wait is bounded by `bound`; a missed bound raises with lifecycle evidence after killing every owned process group."""
    bound, proc, killpg = 30, Path('/proc'), staticmethod(os.killpg)

    def __init__(self, cfg, d):
        self.cfg, self.conn, self.procs = cfg, d.conn, {}
        d.spawn = self.spawn

    def spawn(self, run_id, workdir):
        self.procs[run_id] = subprocess.Popen(
            [sys.executable, '-m', 'hive.runner', '--db', self.cfg['db'], '--run-id', str(run_id)], cwd=str(workdir),
            env=dict(os.environ, PYTHONPATH=str(ROOT)), stdin=subprocess.DEVNULL, start_new_session=True)

    def ws(self, cid):
        return Path(self.cfg['workspaces']) / cid

    def hold(self, cid, rnd):
        self.ws(cid).mkdir(parents=True, exist_ok=True)
        os.mkfifo(self.ws(cid) / f'hold-r{rnd}')
        os.mkfifo(self.ws(cid) / f'ready-r{rnd}')

    def ready(self, cid, rnd):
        got = []
        t = threading.Thread(target=lambda: got.append((self.ws(cid) / f'ready-r{rnd}').read_text()), daemon=True)
        t.start()
        t.join(self.bound)
        if got != [str(rnd)]:
            self.fail(f'held worker round {rnd} of {cid} never started')

    def release(self, cid, rnd):
        fd = os.open(self.ws(cid) / f'hold-r{rnd}', os.O_WRONLY | os.O_NONBLOCK)  # ENXIO: no held head is alive
        os.write(fd, b'go')
        os.close(fd)

    def held(self, cid, rnd):
        try:
            fd = os.open(self.ws(cid) / f'hold-r{rnd}', os.O_WRONLY | os.O_NONBLOCK)
        except OSError as e:
            if e.errno == errno.ENXIO:
                return False
            raise
        os.close(fd)
        return True

    def stopped(self, cid, rnd):
        """Returns once the held round-`rnd` head of `cid` has died, so a stop has reached its process group."""
        end = time.monotonic() + self.bound
        while self.held(cid, rnd):
            if time.monotonic() > end:
                self.fail(f'held worker round {rnd} of {cid} still alive {self.bound}s after its stop')
            time.sleep(0.02)

    def finish(self, run_id):
        try:
            code = self.procs[run_id].wait(timeout=self.bound)
        except subprocess.TimeoutExpired:
            self.fail(f'runner of run {run_id} still alive after {self.bound}s')
        assert code == 0, f'runner of run {run_id} exited {code}'

    def close(self):
        live = {rid: p for rid, p in self.procs.items() if p.poll() is None}
        for p in live.values():
            self.killpg(p.pid, signal.SIGTERM)
        end = time.monotonic() + self.bound
        for rid, p in live.items():
            try:
                p.wait(timeout=max(0, end - time.monotonic()))
            except subprocess.TimeoutExpired:
                self.fail(f'runner of run {rid} still alive {self.bound}s after SIGTERM')
        if self.owned():
            self.fail('processes outlived their runners')

    def owned(self):
        """{pid: pgid} of live processes in a runner's group or working in a card workspace; heads lead their own groups."""
        root = Path(self.cfg['workspaces']).resolve()
        groups = {p.pid for p in self.procs.values() if p.returncode is None}  # a reaped pid may be reused
        found = {}
        for d in self.proc.iterdir():
            if not d.name.isdigit():
                continue
            try:
                state, _, pgid = (d / 'stat').read_text().rsplit(')', 1)[1].split()[:3]
                cwd = Path(os.readlink(d / 'cwd'))
            except (FileNotFoundError, ProcessLookupError, PermissionError):  # exited meanwhile, or another user's
                continue
            if state != 'Z' and (int(pgid) in groups or cwd.is_relative_to(root)):
                found[int(d.name)] = int(pgid)
        return found

    def evidence(self):
        lines = []
        for rid, p in self.procs.items():
            row = self.conn.execute('SELECT status, pid, heartbeat_at, error, run_dir FROM runs WHERE id=?', (rid,)).fetchone()
            lines.append(f'run {rid}: runner pid {p.pid} returncode {p.poll()} row {dict(row) if row else None}')
            log = Path(row['run_dir']) / 'head.log' if row else None
            if log and log.is_file():
                lines.append(f'  head.log tail: {log.read_bytes()[-400:]!r}')
        for pid, pgid in sorted(self.owned().items()):
            try:
                status = dict(ln.split(':\t', 1) for ln in (self.proc / str(pid) / 'status').read_text().splitlines() if ':\t' in ln)
                wchan = (self.proc / str(pid) / 'wchan').read_text()
                cmd = (self.proc / str(pid) / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace').strip()
            except (FileNotFoundError, ProcessLookupError):
                lines.append(f'pid {pid} pgid {pgid}: exited while inspected')
                continue
            lines.append(f"pid {pid} pgid {pgid} ppid {status['PPid']} state {status['State']} wchan {wchan} "
                         f"SigBlk {status['SigBlk']} SigIgn {status['SigIgn']} SigCgt {status['SigCgt']}: {cmd}")
        return '\n'.join(lines)

    def fail(self, what):
        evidence = self.evidence()
        self.kill_all()
        raise AssertionError(f'{what}\n{evidence}')

    def kill_all(self):
        """SIGKILLs every owned process group and reaps the runners; raises if a process survives."""
        for pgid in set(self.owned().values()):
            try:
                self.killpg(pgid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        for p in self.procs.values():
            p.wait(timeout=self.bound)
        end = time.monotonic() + self.bound
        while left := self.owned():
            if time.monotonic() > end:
                raise AssertionError(f'processes survived SIGKILL: {left}')
            time.sleep(0.02)


@pytest.fixture
def live(env):
    cfg, _, d = env
    lv = Live(cfg, d)
    yield lv
    lv.close()


def run_of(conn, cid, rnd):
    return conn.execute("SELECT * FROM runs WHERE card_id=? AND round=? AND role='worker'", (cid, rnd)).fetchone()


def events_of(conn, cid, kind):
    return [e for e in api.get(conn, cid)['events'] if e['kind'] == kind]


def deliveries(conn, cid):
    return [(s['delivery'], s['run']) for s in api.steer_deliveries(conn, cid)]


def test_cli_steer_during_work_hands_off_to_new_head(env, live, monkeypatch, capsys):
    cfg, conn, d = env
    cid = mk(conn, '[ok] steer while working')
    live.hold(cid, 1)
    live.hold(cid, 2)
    d.tick()
    live.ready(cid, 1)
    r1 = run_of(conn, cid, 1)['id']
    text = 'use the v2 endpoint\n  keep `42`'
    assert hive_cli(monkeypatch, cfg, 'steer', cid, text, '--by', 'owner') == 0
    sid = events_of(conn, cid, 'steer')[0]['id']
    assert f'steer {sid} recorded, not delivered yet: worker run {r1} has not seen it' in capsys.readouterr().out
    assert deliveries(conn, cid) == [('stopping', r1)]

    d.tick()
    assert [e['data'] for e in events_of(conn, cid, 'steer_stop')] == [{'run': r1, 'round': 1, 'steers': [sid]}]
    live.finish(r1)
    stopped = run_of(conn, cid, 1)
    assert (stopped['status'], stopped['exit_code'], stopped['error']) == ('exited', -15, 'stopped by SIGTERM')

    d.tick()
    live.ready(cid, 2)
    r2 = run_of(conn, cid, 2)['id']
    assert deliveries(conn, cid) == [('restarted', r2)]
    assert [e['data'] for e in events_of(conn, cid, 'steer_handoff')] == [{'run': r1, 'round': 1, 'run_status': 'exited', 'steers': [sid]}]
    assert claimed_steers(conn, cid) == [None, [sid]]
    prompt = run_of(conn, cid, 2)['prompt']
    assert '## Round 1 was stopped for a clarification' in prompt
    assert f'### Arrived during round 1 (from owner)\n{text}' in prompt
    assert (live.ws(cid) / 'work-r1.txt').read_text() == 'draft of round 1\n'

    live.release(cid, 2)
    live.finish(r2)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    report = workers(c)[1]['report']
    assert 'found: work-r1.txt work-r2.txt' in report and f'### Arrived during round 1 (from owner)\n{text}' in report
    assert deliveries(conn, cid) == [('acknowledged', r2)]
    order = [e['kind'] for e in c['events'] if e['kind'] in ('steer', 'steer_stop', 'run_finished', 'steer_handoff', 'claimed')]
    assert order == ['claimed', 'steer', 'steer_stop', 'run_finished', 'steer_handoff', 'claimed', 'run_finished']
    assert [g['round'] for g in c['gates']] == [2] and [o['kind'] for o in c['outbox']] == ['task_result']
    payload = json.loads(c['outbox'][0]['payload'])
    assert payload['report_excerpt'].startswith('round 2')
    assert payload['steers'] == [{'id': sid, 'by': 'owner', 'delivery': 'acknowledged', 'run': r2}]
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    assert f'steer {sid} by owner: acknowledged (run {r2})' in capsys.readouterr().out


def test_steer_after_worker_exit_before_tick_takes_report_round(env, live):
    cfg, conn, d = env
    cid = mk(conn, '[ok] exit wins the race')
    live.hold(cid, 1)
    d.tick()
    live.ready(cid, 1)
    api.steer(conn, cid, 'add a header row', actor='owner')
    live.release(cid, 1)
    live.finish(run_of(conn, cid, 1)['id'])
    run_heads_inline(cfg, d)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert 'steer_stop' not in kinds(conn, cid) and kinds(conn, cid).count('steer_round') == 1
    assert '### Arrived after the round 1 report (from owner)\nadd a header row' in workers(c)[1]['prompt']
    assert deliveries(conn, cid) == [('acknowledged', workers(c)[1]['id'])] and len(c['outbox']) == 1


def test_stop_racing_normal_exit_hands_off_without_stale_result(env, live):
    cfg, conn, d = env
    cid = mk(conn, '[ok] exit after stop')
    live.hold(cid, 1)
    d.tick()
    live.ready(cid, 1)
    r1 = run_of(conn, cid, 1)['id']
    api.steer(conn, cid, 'name it result.txt', actor='owner')
    signalled = []
    d.kill = lambda r: signalled.append(r['id'])
    d.tick()
    live.release(cid, 1)
    live.finish(r1)
    assert run_of(conn, cid, 1)['report'].startswith('round 1 done') and set(signalled) == {r1}
    run_heads_inline(cfg, d)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert [e['data']['run_status'] for e in events_of(conn, cid, 'steer_handoff')] == ['exited']
    assert [g['round'] for g in c['gates']] == [2] and len(workers(c)) == 2
    assert [o['kind'] for o in c['outbox']] == ['task_result']
    assert json.loads(c['outbox'][0]['payload'])['report_excerpt'].startswith('round 2')


def test_lost_stop_is_visible_then_retried(env, monkeypatch):
    cfg, conn, d = env
    cfg['heartbeat_stale_s'] = 3600  # fake runners never heartbeat
    runs = InProcessRunners(conn, d, monkeypatch)
    cid = mk(conn, '[ok] stop gets lost')
    d.tick()
    r1 = run_of(conn, cid, 1)['id']
    runs.claim(r1)
    api.steer(conn, cid, 'smaller font', actor='owner')
    real_kill, d.kill = d.kill, lambda r: None
    d.tick()
    d.tick()
    assert run_of(conn, cid, 1)['status'] == 'running' and len(events_of(conn, cid, 'steer_stop')) == 1
    stuck = [a for a in dispatcher.observe(conn, cfg, now=time.time() + 120) if a['kind'] == 'steer_stop_stuck']
    assert [(a['card'], a['run']) for a in stuck] == [(cid, r1)]
    d.kill = real_kill
    d.tick()
    assert runs.sent == [(runs.runners[r1].pid, signal.SIGTERM)] and runs.runners[r1].wait() == 0
    assert run_of(conn, cid, 1)['error'] == 'stopped by SIGTERM'
    r2 = run_of(conn, cid, 2)['id']
    runs.claim(r2)
    runs.work(r2)
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert len(events_of(conn, cid, 'steer_stop')) == 1 and len(events_of(conn, cid, 'steer_handoff')) == 1
    assert [e['data']['run_status'] for e in events_of(conn, cid, 'steer_handoff')] == ['exited']
    assert not [a for a in dispatcher.observe(conn, cfg, now=time.time() + 120) if a['kind'] == 'steer_stop_stuck']
    assert deliveries(conn, cid) == [('acknowledged', r2)] and runs.sent == [(runs.runners[r1].pid, signal.SIGTERM)]


def test_two_steers_one_handoff_bystander_untouched(env, live):
    cfg, conn, d = env
    cid = mk(conn, '[ok] two live steers')
    other = mk(conn, '[ok] live bystander')
    for card in (cid, other):
        live.hold(card, 1)
    d.tick()
    live.ready(cid, 1)
    live.ready(other, 1)
    r1, rb = run_of(conn, cid, 1)['id'], run_of(conn, other, 1)['id']
    s1 = api.steer(conn, cid, 'first: plain text', actor='owner')
    d.tick()
    s2 = api.steer(conn, cid, 'second: with a table', actor='secretary')
    live.finish(r1)
    for _ in range(3):
        d.tick()
    assert live.procs[rb].poll() is None and run_of(conn, other, 1)['status'] == 'running'
    assert api.get(conn, other)['state'] == 'running'
    assert [e['data']['steers'] for e in events_of(conn, cid, 'steer_stop')] == [[s1]]
    handoffs = events_of(conn, cid, 'steer_handoff')  # s2 is in it unless the stopped run was handled within the stopping tick
    assert len(handoffs) == 1 and handoffs[0]['data']['steers'][0] == s1
    r2 = run_of(conn, cid, 2)['id']
    live.finish(r2)
    assert tick_until_terminal(d, conn, cid) == 'done'
    live.release(other, 1)
    live.finish(rb)
    assert tick_until_terminal(d, conn, other) == 'done'
    for _ in range(3):
        d.tick()
    c = api.get(conn, cid)
    prompt = workers(c)[1]['prompt']
    assert prompt.index('first: plain text') < prompt.index('second: with a table')
    assert '### Arrived during round 1 (from secretary)\nsecond: with a table' in prompt
    assert claimed_steers(conn, cid) == [None, [s1, s2]] and len(workers(c)) == 2 and len(c['outbox']) == 1
    assert deliveries(conn, cid) == [('acknowledged', r2), ('acknowledged', r2)]
    b = api.get(conn, other)
    assert b['round'] == 1 and len(b['runs']) == 1 and b['runs'][0]['error'] is None and b['steers'] == []
    assert not {'steer_stop', 'steer_handoff', 'steer_round'} & set(kinds(conn, other))


def test_steer_during_single_failure_budget_finishes(env, live):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] active steer with one failure allowed')
    live.hold(cid, 1)
    d.tick()
    live.ready(cid, 1)
    sid = api.steer(conn, cid, 'switch to csv', actor='owner')
    d.tick()
    live.stopped(cid, 1)
    live.finish(run_of(conn, cid, 1)['id'])
    assert run_until(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert len(workers(c)) == 2 and [g['passed'] for g in c['gates']] == [1]
    assert [o['kind'] for o in c['outbox']] == ['task_result']
    assert json.loads(c['outbox'][0]['payload'])['steers'] == [
        {'id': sid, 'by': 'owner', 'delivery': 'acknowledged', 'run': workers(c)[1]['id']}]


class FakeRunner:
    def __init__(self, pid):
        self.pid, self.returncode = pid, None

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        if self.returncode is None:
            raise subprocess.TimeoutExpired('runner', timeout)
        return self.returncode


class InProcessRunners:
    """Dispatcher launches start no process: each gets a FakeRunner the test claims and ends with the run row its
    runner would write. `os.killpg` reaches only these runners; a SIGTERM ends the run as a stopped head. A run's
    process lives while its runner has not ended."""
    def __init__(self, conn, d, monkeypatch):
        self.conn, self.runners, self.sent = conn, {}, []
        d.spawn = lambda rid, wd: self.runners.setdefault(rid, FakeRunner(4000 + rid))
        monkeypatch.setattr(os, 'killpg', self.killpg)
        monkeypatch.setattr(liveness, 'alive', self.alive)

    def alive(self, run, cgroup_root):
        return run['id'] in self.runners and self.runners[run['id']].returncode is None

    def claim(self, rid):
        cur = self.conn.execute("UPDATE runs SET status='running', pid=?, heartbeat_at=? WHERE id=? AND status='starting'",
                                (self.runners[rid].pid, time.time(), rid))
        assert cur.rowcount == 1, f'run {rid} not claimable'

    def job(self, rid):
        return json.loads((Path(self.run(rid)['run_dir']) / 'job.json').read_text())

    def run(self, rid):
        return self.conn.execute('SELECT * FROM runs WHERE id=?', (rid,)).fetchone()

    def work(self, rid):
        """The head of an `[ok]` card writes out.txt and a report quoting its prompt's owner clarifications."""
        job = self.job(rid)
        Path(job['cwd'], 'out.txt').write_text('42\n')
        prompt = job['prompt']
        report = f"round {self.run(rid)['round']} done\n{prompt[prompt.index('## Owner clarifications'):]}"
        self.end(rid, 0, None, report, 'file')

    def killpg(self, pgid, sig):
        [rid] = [rid for rid, p in self.runners.items() if p.pid == pgid and p.returncode is None]
        self.sent.append((pgid, sig))
        assert sig == signal.SIGTERM
        self.end(rid, -signal.SIGTERM, 'stopped by SIGTERM', None, 'none')

    def end(self, rid, code, err, report, source):
        now = time.time()
        with store.tx(self.conn):
            cur = self.conn.execute("""UPDATE runs SET status='exited', exit_code=?, ended_at=?, outcome=?, report=?, error=?,
                                       heartbeat_at=? WHERE id=? AND status='running'""",
                                    (code, now, 'ok' if code == 0 else f'exit {code}', report or '', err, now, rid))
            assert cur.rowcount == 1, f'run {rid} not running'
            store.event(self.conn, self.run(rid)['card_id'], 'run_report',
                        {'run': rid, 'file': self.job(rid)['report_file'], 'source': source, 'inherited': None}, actor='runner')
        self.runners[rid].returncode = 0


def test_more_active_steers_than_failure_budget_finish_once(env, live):
    cfg, conn, d = env
    cfg['max_rounds'] = 2
    cid = mk(conn, '[ok] successive clarifications')
    for rnd in range(1, 5):
        live.hold(cid, rnd)
    d.tick()
    ids = []
    for rnd in range(1, 4):
        live.ready(cid, rnd)
        before = len(events_of(conn, cid, 'steer_stop'))
        d.tick()
        d.tick()
        assert len(events_of(conn, cid, 'steer_stop')) == before
        ids.append(api.steer(conn, cid, f'include column {rnd}', actor='owner'))
        d.tick()
        live.finish(run_of(conn, cid, rnd)['id'])
        d.tick()
    live.ready(cid, 4)
    d.tick()
    assert len(events_of(conn, cid, 'steer_stop')) == 3
    live.release(cid, 4)
    last = run_of(conn, cid, 4)['id']
    live.finish(last)
    assert tick_until_terminal(d, conn, cid) == 'done'
    for _ in range(3):
        d.tick()
    c = api.get(conn, cid)
    assert [r['round'] for r in workers(c)] == [1, 2, 3, 4]
    assert len({r['run_dir'] for r in workers(c)}) == 4
    assert all(Path(r['run_dir'], 'job.json').is_file() for r in workers(c))
    assert claimed_steers(conn, cid) == [None, ids[:1], ids[:2], ids]
    assert [e['data']['steers'] for e in events_of(conn, cid, 'steer_stop')] == [[i] for i in ids]
    assert len(events_of(conn, cid, 'steer_handoff')) == 3
    assert deliveries(conn, cid) == [('acknowledged', last)] * 3
    assert events_of(conn, cid, 'steer_check')[-1]['data']['acknowledged'] == ids
    assert [s['delivery'] for s in json.loads(c['outbox'][0]['payload'])['steers']] == ['acknowledged'] * 3


def test_more_late_steers_than_failure_budget_are_each_acknowledged(env):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] successive late clarifications', review=True)
    ids = []
    d.advance()
    for n in range(3):
        d.launch()
        ids.append(api.steer(conn, cid, f'append section {n}', actor='owner'))
        d.advance()
        assert api.get(conn, cid)['state'] == 'queued'
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert claimed_steers(conn, cid) == [None] + [[i] for i in ids]
    assert [e['data']['acknowledged'] for e in events_of(conn, cid, 'steer_check')] == [[i] for i in ids]
    assert len(workers(c)) == 4 and len(events_of(conn, cid, 'steer_round')) == 3
    assert all(s['delivery'] == 'acknowledged' for s in c['steers'])
    assert c['runs'][-1]['verdict'] == 'PASS' and len(c['outbox']) == 1


def test_steer_stop_recovers_once_across_dispatcher_restart(env, monkeypatch):
    cfg, conn, d = env
    runs = InProcessRunners(conn, d, monkeypatch)
    cid = mk(conn, '[ok] restart during stop')
    d.tick()
    first = run_of(conn, cid, 1)['id']
    runs.claim(first)
    sid = api.steer(conn, cid, 'include the units', actor='owner')
    d.kill = lambda r: None
    d.tick()
    with store.connect(cfg['db']) as reopened:
        fresh = dispatcher.Dispatcher(cfg, reopened, limits_fetcher=lambda c: {})
        fresh.spawn = d.spawn
        fresh.tick()
        second = run_of(conn, cid, 2)['id']
        runs.claim(second)
        fresh.tick()
        assert deliveries(conn, cid) == [('restarted', second)]
        runs.work(second)
        assert tick_until_terminal(fresh, conn, cid) == 'done'
        fresh.tick()
    assert claimed_steers(conn, cid) == [None, [sid]]
    assert len(events_of(conn, cid, 'steer_stop')) == len(events_of(conn, cid, 'steer_handoff')) == 1
    assert runs.sent == [(runs.runners[first].pid, signal.SIGTERM)]
    assert deliveries(conn, cid) == [('acknowledged', second)]


def test_provider_failover_claim_sees_steer_in_same_round(env, monkeypatch):
    cfg, conn, d = env
    runs = InProcessRunners(conn, d, monkeypatch)
    cid = mk(conn, '[ok] steer between provider attempts')
    d.tick()
    first = workers(api.get(conn, cid))[0]['id']
    runs.claim(first)
    runs.end(first, 1, 'HTTP 429', None, 'none')
    conn.execute("UPDATE runs SET provider_failure='http_429' WHERE id=?", (first,))
    sid = api.steer(conn, cid, 'use the archive', actor='owner')
    d.tick()
    second = workers(api.get(conn, cid))[1]['id']
    runs.claim(second)
    d.tick()
    assert runs.run(second)['status'] == 'running' and runs.sent == []
    assert claimed_steers(conn, cid) == [None, [sid]]
    assert runs.run(first)['round'] == runs.run(second)['round'] == 1
    runs.work(second)
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert deliveries(conn, cid) == [('acknowledged', second)]


def test_review_failures_after_late_steer_spend_failure_budget(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [reviewfail] steered review', review=True)
    d.advance()
    d.launch()
    sid = api.steer(conn, cid, 'check all totals', actor='owner')
    d.advance()
    assert tick_until_terminal(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    assert len(workers(c)) == 3 and c['block_reason'].startswith('judge rework')
    assert [r['verdict'] for r in c['runs'] if r['role'] == 'reviewer'] == ['FAIL', 'FAIL']
    assert len(events_of(conn, cid, 'retry')) == 1
    assert json.loads(c['outbox'][0]['payload'])['steers'] == [
        {'id': sid, 'by': 'owner', 'delivery': 'acknowledged', 'run': workers(c)[1]['id']}]


@pytest.mark.parametrize('failure', ['crash', 'timeout', 'exit', 'gate', 'noquote'])
def test_real_failures_after_handoff_exhaust_only_failure_budget(env, monkeypatch, failure):
    cfg, conn, d = env
    cfg['max_rounds'] = 2
    runs = InProcessRunners(conn, d, monkeypatch)
    cid = mk(conn, '[ok] real failures after a steer')
    if failure == 'gate':
        conn.execute("UPDATE cards SET acceptance='false' WHERE id=?", (cid,))
    d.tick()
    first = run_of(conn, cid, 1)['id']
    runs.claim(first)
    sid = api.steer(conn, cid, 'show the totals', actor='owner')
    d.tick()
    for rnd in (2, 3):
        rid = run_of(conn, cid, rnd)['id']
        runs.claim(rid)
        if failure == 'crash':
            Path(runs.run(rid)['run_dir'], 'head.log').write_text('work started\n')
            conn.execute('UPDATE runs SET heartbeat_at=? WHERE id=?', (d.clock() - 10, rid))
            d.alive = lambda r: False
        elif failure == 'timeout':
            conn.execute('UPDATE runs SET started_at=? WHERE id=?', (d.clock() - cfg['timeouts'][1] - 200, rid))
            d.kill = lambda r: None
        elif failure == 'exit':
            runs.work(rid)
            conn.execute("UPDATE runs SET exit_code=1, outcome='exit 1', error='task failed' WHERE id=?", (rid,))
        elif failure == 'noquote':
            runs.end(rid, 0, None, f'round {rnd} finished without quoting', 'file')
        else:
            runs.work(rid)
        d.tick()
        assert api.get(conn, cid)['state'] == ('running' if rnd == 2 else 'blocked')
    c = api.get(conn, cid)
    assert len(workers(c)) == 3 and len(events_of(conn, cid, 'retry')) == 1
    assert len(events_of(conn, cid, 'steer_handoff')) == 1
    payload = json.loads(c['outbox'][0]['payload'])
    assert payload['state'] == 'blocked'
    assert payload['steers'][0]['id'] == sid
    if failure == 'noquote':
        assert payload['steers'][0]['delivery'] == 'unacknowledged'
        assert [e['data']['missing'] for e in events_of(conn, cid, 'steer_check')] == [[sid], [sid]]
    if failure in ('crash', 'timeout'):
        assert [r['status'] for r in workers(c)[1:]] == [failure if failure == 'timeout' else 'crashed'] * 2
        assert all(r['provider_failure'] is None for r in workers(c))


def fake_proc(root, pid, ppid, pgid, cwd, cmdline):
    d = root / str(pid)
    d.mkdir(parents=True)
    (d / 'stat').write_text(f'{pid} (x) S {ppid} {pgid} {pgid} 0')
    (d / 'cwd').symlink_to(cwd.resolve())
    (d / 'status').write_text(f'Name:\tx\nState:\tS (sleeping)\nPPid:\t{ppid}\nSigBlk:\t0\nSigIgn:\t4000\nSigCgt:\t0\n')
    (d / 'wchan').write_text('pipe_read')
    (d / 'cmdline').write_bytes(cmdline)


@pytest.mark.parametrize('end, missed', [(lambda lv: lv.finish(7), 'runner of run 7 still alive after 30s'),
                                          (Live.close, 'runner of run 7 still alive 30s after SIGTERM')])
def test_live_missed_bound_reports_and_kills_owned_groups(live, tmp_path, end, missed):
    """A runner and its head that ignore SIGTERM: the missed bound reports both and SIGKILLs only their groups."""
    ws, other = live.ws('stuck'), tmp_path / 'elsewhere'
    ws.mkdir(parents=True)
    other.mkdir()
    live.proc = tmp_path / 'proc'
    fake_proc(live.proc, 4100, 1, 4100, ws, b'python\0-m\0hive.runner\0')
    fake_proc(live.proc, 4101, 4100, 4101, ws, b'fakehead\0hold-r1\0')
    fake_proc(live.proc, 4200, 1, 4200, other, b'bystander\0')
    (live.proc / 'self').mkdir()
    live.procs[7] = runner = FakeRunner(4100)
    sent = []

    def killpg(pgid, sig):
        sent.append((pgid, sig))
        if sig == signal.SIGKILL:
            for d in [d for d in live.proc.iterdir() if d.name.isdigit() and live.owned().get(int(d.name)) == pgid]:
                shutil.rmtree(d)
            if pgid == runner.pid:
                runner.returncode = -signal.SIGKILL
    live.killpg = killpg
    with pytest.raises(AssertionError) as e:
        end(live)
    report = str(e.value)
    assert report.startswith(missed) and 'run 7: runner pid 4100 returncode None row None' in report
    assert ('pid 4101 pgid 4101 ppid 4100 state S (sleeping) wchan pipe_read SigBlk 0 SigIgn 4000 SigCgt 0: fakehead hold-r1'
            in report) and 'pid 4200' not in report
    term = [(4100, signal.SIGTERM)] if end is Live.close else []
    assert sent[:len(term)] == term and sorted(sent[len(term):]) == [(4100, signal.SIGKILL), (4101, signal.SIGKILL)]
    assert runner.returncode == -signal.SIGKILL and live.owned() == {} and (live.proc / '4200').is_dir()


def test_steer_before_runner_claims_run_cancels_that_head(env):
    cfg, conn, d = env
    cid = mk(conn, '[ok] steer before start')
    d.advance()
    d.spawn = lambda rid, wd: None
    d.launch()
    r1 = run_of(conn, cid, 1)['id']
    api.steer(conn, cid, 'start from the template', actor='owner')
    d.advance()
    assert run_of(conn, cid, 1)['status'] == 'interrupted' and api.get(conn, cid)['state'] == 'queued'
    from hive import runner
    assert runner.main(['--db', cfg['db'], '--run-id', str(r1)]) == 3
    run_heads_inline(cfg, d)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert [e['data']['run_status'] for e in events_of(conn, cid, 'steer_handoff')] == ['interrupted']
    assert '### Arrived during round 1 (from owner)\nstart from the template' in workers(c)[1]['prompt']
    assert deliveries(conn, cid) == [('acknowledged', workers(c)[1]['id'])]


def test_report_without_clarification_retries_with_it(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [noquote-r1] forgets the steer')
    sid = api.steer(conn, cid, 'round the totals', actor='owner')
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    r1, r2 = workers(c)
    assert claimed_steers(conn, cid) == [[sid], [sid]] and [g['round'] for g in c['gates']] == [2]
    assert [e['data'] for e in events_of(conn, cid, 'steer_check')] == [
        {'run': r1['id'], 'round': 1, 'fresh_report': True, 'missing': [sid], 'acknowledged': []},
        {'run': r2['id'], 'round': 2, 'fresh_report': True, 'missing': [], 'acknowledged': [sid]}]
    assert 'REPORT.md did not quote these owner clarifications' in r2['prompt'] and 'From owner:\nround the totals' in r2['prompt']
    assert json.loads(c['outbox'][0]['payload'])['steers'] == [{'id': sid, 'by': 'owner', 'delivery': 'acknowledged', 'run': r2['id']}]


def test_handoff_head_without_report_blocks_visibly(env, live):
    cfg, conn, d = env
    cfg['max_rounds'] = 1
    cid = mk(conn, '[ok] [noreport-r2] silent replacement')
    live.hold(cid, 1)
    d.tick()
    live.ready(cid, 1)
    sid = api.steer(conn, cid, 'use metric units', actor='owner')
    d.tick()
    live.finish(run_of(conn, cid, 1)['id'])
    run_heads_inline(cfg, d)
    assert tick_until_terminal(d, conn, cid) == 'blocked'
    c = api.get(conn, cid)
    r2 = run_of(conn, cid, 2)
    assert (r2['status'], r2['exit_code']) == ('exited', 0) and (live.ws(cid) / 'out.txt').is_file()
    assert c['block_reason'] == f'round 2 report does not quote clarification(s) [{sid}]' and c['gates'] == []
    assert [o['kind'] for o in c['outbox']] == ['task_blocked']
    assert json.loads(c['outbox'][0]['payload'])['steers'] == [{'id': sid, 'by': 'owner', 'delivery': 'unacknowledged', 'run': r2['id']}]


@pytest.mark.parametrize('title, inline, delivery, words', [
    ('[ok] claimed before the read', False, 'handed_off', 'handed off to worker run {run} in its prompt, not acknowledged until its report quotes it'),
    ('[ok] quoted before the read', True, 'acknowledged', 'acknowledged: the report of worker run {run} quotes it'),
    ('[ok] [noquote-r1] unquoted before the read', True, 'unacknowledged',
     'not acknowledged: worker run {run} ended without quoting it in its report; the next worker round must quote it or the card blocks'),
])
def test_cli_steer_reports_state_the_dispatcher_reached_after_commit(env, monkeypatch, capsys, title, inline, delivery, words):
    cfg, conn, d = env
    if inline:
        run_heads_inline(cfg, d)
    else:
        d.spawn = lambda rid, wd: None
    cid = mk(conn, title)
    real_steer = api.steer

    def steer_then_dispatch(*a, **kw):
        sid = real_steer(*a, **kw)
        d.advance()
        d.launch()
        return sid
    monkeypatch.setattr(api, 'steer', steer_then_dispatch)
    assert hive_cli(monkeypatch, cfg, 'steer', cid, 'only weekdays', '--by', 'owner') == 0
    run = run_of(conn, cid, 1)['id']
    sid = events_of(conn, cid, 'steer')[0]['id']
    assert deliveries(conn, cid) == [(delivery, run)]
    assert f"{cid} steer {sid} recorded, {words.format(run=run)}." in capsys.readouterr().out


def test_inherited_report_quoting_the_steer_is_not_an_acknowledgment(env, live):
    cfg, conn, d = env
    cfg['max_rounds'] = 3
    cid = mk(conn, '[ok] [noreport-r2] stale report')
    live.hold(cid, 1)
    d.tick()
    live.ready(cid, 1)
    text = 'ship it as csv'
    old = live.ws(cid) / 'REPORT.md'
    old.write_text(f'round 1 notes\n{text}\n')
    os.utime(old, (time.time() + 3600, time.time() + 3600))  # newer than any later run start
    sid = api.steer(conn, cid, text, actor='owner')
    d.tick()
    r1 = run_of(conn, cid, 1)['id']
    live.finish(r1)
    run_heads_inline(cfg, d)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    r2, r3 = run_of(conn, cid, 2), run_of(conn, cid, 3)
    reports = {e['data']['run']: e['data'] for e in events_of(conn, cid, 'run_report')}
    assert (reports[r1]['source'], reports[r1]['inherited']) == ('file', None)
    assert reports[r2['id']]['source'] != 'file' and text not in r2['report']
    assert Path(reports[r2['id']]['inherited']).read_text() == f'round 1 notes\n{text}\n'
    assert reports[r3['id']] == {'run': r3['id'], 'file': 'REPORT.md', 'source': 'file', 'inherited': None}
    assert [(e['data']['run'], e['data']['fresh_report'], e['data']['missing']) for e in events_of(conn, cid, 'steer_check')] == [
        (r2['id'], False, [sid]), (r3['id'], True, [])]
    assert [g['round'] for g in c['gates']] == [3] and claimed_steers(conn, cid) == [None, [sid], [sid]]
    assert deliveries(conn, cid) == [('acknowledged', r3['id'])]
    assert json.loads(c['outbox'][0]['payload'])['steers'] == [{'id': sid, 'by': 'owner', 'delivery': 'acknowledged', 'run': r3['id']}]


# ── assignee ────────────────────────────────────────────────────────────────

EXECUTORS = ('claude', 'codex')


def worker_vendors(conn, cid):
    return [r['vendor'] for r in workers(api.get(conn, cid))]


def test_assignment_round_trips_in_get_listing_and_cli(env, monkeypatch, capsys):
    cfg, conn, _ = env
    cid = mk(conn, '[ok] assigned', assignee='codex', on_unavailable='fallback', not_before=5000.0,
             executors=EXECUTORS)
    card = api.get(conn, cid)
    assert (card['assignee'], card['on_unavailable'], card['not_before'], card['executor_wait']) == (
        'codex', 'fallback', 5000.0, None)
    plain = api.get(conn, mk(conn, '[ok] plain'))
    assert (plain['assignee'], plain['on_unavailable'], plain['executor_wait']) == (None, 'wait', None)
    assert {r['id']: r['assignee'] for r in api.listing(conn)}[cid] == 'codex'
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] cli assigned', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'system:test', '--created-by', 'secretary', '--assignee', 'claude') == 0
    cli = capsys.readouterr().out.split()[0]
    assert (api.get(conn, cli)['assignee'], api.get(conn, cli)['on_unavailable']) == ('claude', 'wait')
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    assert 'assignee: codex · on unavailable: fallback · not before: ' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'list') == 0
    assert '@claude' in next(line for line in capsys.readouterr().out.splitlines() if line.startswith(cli))


def test_assignment_rejections_write_nothing(env, monkeypatch, capsys):
    cfg, conn, _ = env
    for kw in ({'assignee': 'dots'}, {'assignee': 'claude', 'on_unavailable': 'never'}):
        with pytest.raises(api.CardError):
            mk(conn, '[ok] rejected', executors=EXECUTORS, **kw)
    with pytest.raises(api.CardError):
        mk_epic(conn, '[ok] epic', assignee='claude', executors=EXECUTORS)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 0
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 't', '--goal', 'g', '--acceptance', 'true', '--step', '1',
                    '--source', 'system:test', '--created-by', 'secretary', '--assignee', 'dots') == 2
    assert 'unknown executor' in capsys.readouterr().err
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 0

    cid = mk(conn, '[ok] target')
    epic = mk_epic(conn, '[ok] epic')
    running = mk(conn, '[ok] running')
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (running,))
    failed = mk(conn, '[ok] failed')
    settle(conn, failed, 'blocked', 'gate exit 1')
    before = conn.execute('SELECT count(*) FROM events').fetchone()[0]
    for card, assignee, policy in ((cid, 'dots', 'wait'), (cid, 'claude', 'never'), (epic, 'claude', 'wait'),
                                   (running, 'claude', 'wait'), (failed, 'claude', 'wait')):
        with pytest.raises(api.CardError):
            api.assign(conn, card, assignee, on_unavailable=policy, executors=EXECUTORS)
    assert hive_cli(monkeypatch, cfg, 'assign', running, 'codex') == 2
    assert 'hive: assignment rejected' in capsys.readouterr().err
    assert hive_cli(monkeypatch, cfg, 'assign', cid, 'claude', '--on-unavailable', 'never') == 2
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == before
    assert [api.get(conn, c)['assignee'] for c in (cid, running, failed)] == [None, None, None]


def test_assign_changes_are_journaled_once_and_allowed_while_waiting(env, monkeypatch, capsys):
    cfg, conn, d = env
    record_spawns(d)
    blocker = mk(conn, '[ok] blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] waiter', blocked_by=[blocker])
    assert api.assign(conn, cid, 'codex', executors=EXECUTORS)
    assert not api.assign(conn, cid, 'codex', executors=EXECUTORS)
    assert api.assign(conn, cid, 'codex', on_unavailable='fallback', executors=EXECUTORS)
    assert hive_cli(monkeypatch, cfg, 'assign', cid, '--none', '--on-unavailable', 'fallback') == 0
    assert 'changed assignee=—' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'assign', cid, '--none', '--on-unavailable', 'fallback') == 0
    assert 'unchanged' in capsys.readouterr().out
    assigned = [e['data'] for e in api.get(conn, cid)['events'] if e['kind'] == 'assigned']
    assert [(a['assignee'], a['on_unavailable']) for a in assigned] == [
        ('codex', 'wait'), ('codex', 'fallback'), (None, 'fallback')]
    assert api.get(conn, cid)['state'] == 'blocked'
    d.tick()
    queued = mk(conn, '[ok] queued', not_before=time.time() + 3600)
    conn.execute("UPDATE cards SET state='queued' WHERE id=?", (queued,))
    assert api.assign(conn, queued, 'claude', executors=EXECUTORS)


def test_assigned_worker_runs_only_on_its_executor(env):
    cfg, conn, d = env
    cfg['concurrency'] = 4
    spawned = record_spawns(d)
    d.limits_fetcher = lambda c: {'claude': 90.0, 'codex': 40.0}
    assigned = [mk(conn, f'[ok] pinned {i}', assignee='codex', executors=EXECUTORS) for i in range(3)]
    plain = mk(conn, '[ok] free choice')
    d.tick()
    assert len(spawned) == 4
    assert [worker_vendors(conn, cid) for cid in assigned] == [['codex']] * 3
    assert worker_vendors(conn, plain) == ['claude']
    assert all('assignee_fallback' not in kinds(conn, cid) for cid in assigned)


def test_not_before_holds_assigned_card(env):
    _, conn, d = env
    spawned = record_spawns(d)
    now = [1000.0]
    d.clock = lambda: now[0]
    cid = mk(conn, '[ok] later', assignee='codex', not_before=2000.0, executors=EXECUTORS)
    d.tick()
    assert api.get(conn, cid)['state'] == 'created' and spawned == []
    now[0] = 2001.0
    d.tick()
    assert worker_vendors(conn, cid) == ['codex']


def test_assignment_survives_dependency_wait_and_new_dispatcher(env):
    cfg, conn, _ = env
    blocker = mk(conn, '[ok] blocker', not_before=time.time() + 3600)
    cid = mk(conn, '[ok] dependent', blocked_by=[blocker], assignee='codex', executors=EXECUTORS)
    settle(conn, blocker, 'done')
    conn.close()
    fresh = store.connect(cfg['db'])
    d = dispatcher.Dispatcher(cfg, fresh, limits_fetcher=lambda c: {'claude': 90.0, 'codex': 40.0})
    record_spawns(d)
    d.advance()
    d.advance()
    d.launch()
    assert worker_vendors(fresh, cid) == ['codex']


def test_assignment_holds_in_retry_round(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    d.limits_fetcher = lambda c: {'claude': 90.0, 'codex': 40.0}
    cid = mk(conn, '[round2] pinned retry', assignee='codex', executors=EXECUTORS)
    assert tick_until_terminal(d, conn, cid) == 'done'
    c = api.get(conn, cid)
    assert c['round'] == 2 and [(r['round'], r['vendor']) for r in workers(c)] == [(1, 'codex'), (2, 'codex')]


def test_provider_failure_of_assignee_waits_without_failover(env):
    cfg, conn, d = env
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 10}
    now = [1000.0]
    d.clock = lambda: now[0]
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] [provider429] pinned outage', assignee='claude', executors=EXECUTORS)
    d.tick()
    d.tick()
    c = api.get(conn, cid)
    assert c['state'] == 'queued' and c['round'] == 1 and worker_vendors(conn, cid) == ['claude']
    assert c['executor_wait']['executor'] == 'claude' and 'cooldown' in c['executor_wait']['reason']
    assert 'provider_failover' not in kinds(conn, cid) and 'retry' not in kinds(conn, cid)
    assert 'assignee_provider_wait' in kinds(conn, cid)
    now[0] += 10
    assert tick_until_terminal(d, conn, cid) == 'done'
    done = api.get(conn, cid)
    assert [(r['round'], r['vendor']) for r in workers(done)] == [(1, 'claude'), (1, 'claude')]
    assert done['round'] == 1 and done['executor_wait'] is None


def test_wait_policy_parks_card_lets_others_run_and_launches_on_recovery(env, monkeypatch, capsys):
    cfg, conn, d = env
    cfg['concurrency'] = 1
    spend_daily(conn, cfg, 'codex')
    spawned = record_spawns(d)
    pinned = mk(conn, '[ok] pinned', assignee='codex', priority=5, executors=EXECUTORS)
    other = mk(conn, '[ok] other')
    d.tick()
    d.tick()
    card = api.get(conn, pinned)
    assert (card['state'], card['round'], card['runs']) == ('queued', 0, [])
    assert card['executor_wait'] == {'executor': 'codex', 'reason': 'daily runs limit 1 reached'}
    assert worker_vendors(conn, other) == ['claude'] and len(spawned) == 1
    assert kinds(conn, pinned).count('executor_wait') == 1 and 'budget_wait' not in kinds(conn, pinned)
    assert {'kind': 'executor_wait', 'card': pinned, 'executor': 'codex',
            'reason': 'daily runs limit 1 reached'} in dispatcher.observe(conn, cfg)
    assert hive_cli(monkeypatch, cfg, 'show', pinned) == 0
    assert 'waiting for executor codex: daily runs limit 1 reached' in capsys.readouterr().out

    cfg['concurrency'] = 2
    cfg['vendors']['codex']['disabled'] = True
    d.tick()
    assert [e['data'] for e in api.get(conn, pinned)['events'] if e['kind'] == 'executor_wait'] == [
        {'executor': 'codex', 'reason': 'daily runs limit 1 reached'}, {'executor': 'codex', 'reason': 'disabled'}]

    cfg['vendors']['codex'].update(disabled=False, daily_runs=100)
    d.tick()
    card = api.get(conn, pinned)
    assert worker_vendors(conn, pinned) == ['codex'] and card['round'] == 1 and card['executor_wait'] is None
    assert not [a for a in dispatcher.observe(conn, cfg) if a['kind'] == 'executor_wait']


def test_fallback_policy_takes_ladder_pick_with_event_and_keeps_assignment(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    spend_daily(conn, cfg, 'codex')
    cid = mk(conn, '[round2] pinned fallback', assignee='codex', on_unavailable='fallback', executors=EXECUTORS)
    d.tick()
    c = api.get(conn, cid)
    assert c['state'] == 'queued' and c['round'] == 1 and worker_vendors(conn, cid) == ['claude']
    assert [e['data'] for e in c['events'] if e['kind'] == 'assignee_fallback'] == [
        {'run': workers(c)[0]['id'], 'round': 1, 'assigned': 'codex', 'actual': 'claude',
         'reason': 'daily runs limit 1 reached'}]
    assert c['assignee'] == 'codex' and c['executor_wait'] is None
    cfg['vendors']['codex']['daily_runs'] = 100
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert worker_vendors(conn, cid) == ['claude', 'codex']
    assert kinds(conn, cid).count('assignee_fallback') == 1


def test_fallback_provider_failure_switches_with_assignment_trail(env):
    cfg, conn, d = env
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 10}
    now = [1000.0]
    d.clock = lambda: now[0]
    run_heads_inline(cfg, d)
    cid = mk(conn, '[round2] [provider429] pinned fallback outage', assignee='claude', on_unavailable='fallback',
             executors=EXECUTORS)
    d.tick()
    d.tick()
    c = api.get(conn, cid)
    failed, switched = workers(c)[:2]
    assert (failed['vendor'], failed['provider_failure'], switched['vendor']) == ('claude', 'http_429', 'codex')
    assert [e['data'] for e in c['events'] if e['kind'] == 'assignee_fallback'] == [
        {'run': switched['id'], 'round': 1, 'assigned': 'claude', 'actual': 'codex',
         'reason': f"provider failure http_429 of run {failed['id']}"}]
    assert c['assignee'] == 'claude' and 'assignee_provider_wait' not in kinds(conn, cid)
    now[0] += 10
    assert tick_until_terminal(d, conn, cid) == 'done'
    done = api.get(conn, cid)
    assert [(r['round'], r['vendor']) for r in workers(done)] == [(1, 'claude'), (1, 'codex'), (2, 'claude')]
    assert kinds(conn, cid).count('assignee_fallback') == 1


def test_configured_executor_the_dispatcher_cannot_run_is_refused(env, monkeypatch, capsys, tmp_path):
    cfg, conn, _ = env
    cfg['vendors']['dots'] = dict(cfg['vendors']['claude'])
    for step in cfg['ladder']:
        cfg['ladder'][step]['dots'] = dict(cfg['ladder'][step]['claude'])
    with pytest.raises(api.CardError):
        mk(conn, '[ok] dots', assignee='dots', executors=heads.executors(cfg))
    cid = mk(conn, '[ok] target')
    events = conn.execute('SELECT count(*) FROM events').fetchone()[0]
    with pytest.raises(api.CardError):
        api.assign(conn, cid, 'dots', executors=heads.executors(cfg))
    config = tmp_path / 'with-dots.toml'
    config.write_text(Path(cfg['_path']).read_text() + '\n[vendors.dots]\nweight = 1.0\nbinary = ["dots"]\n')
    for argv in (('create', '--title', 't', '--goal', 'g', '--acceptance', 'true', '--step', '1', '--source', 'system:test',
                  '--created-by', 'secretary', '--assignee', 'dots'), ('assign', cid, 'dots')):
        assert hive_cli(monkeypatch, cfg, '--config', str(config), *argv) == 2
        assert 'unknown executor' in capsys.readouterr().err
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 1
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == events
    assert api.get(conn, cid)['assignee'] is None

def test_legacy_cards_migrate_without_assignment_and_launch_as_before(env, tmp_path):
    cfg, _, _ = env
    db = tmp_path / 'legacy-assign.db'
    conn = sqlite3.connect(db)
    conn.execute("""CREATE TABLE cards(
        id TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'task', lane TEXT NOT NULL DEFAULT 'task',
        title TEXT NOT NULL, goal TEXT NOT NULL, acceptance TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
        step INTEGER NOT NULL, review INTEGER NOT NULL DEFAULT 0, task_type TEXT NOT NULL DEFAULT 'general',
        source TEXT NOT NULL, created_by TEXT NOT NULL, idem_key TEXT UNIQUE, priority INTEGER NOT NULL DEFAULT 0,
        not_before REAL, state TEXT NOT NULL, round INTEGER NOT NULL DEFAULT 0, version INTEGER NOT NULL DEFAULT 0,
        workdir TEXT, block_reason TEXT, wait_reason TEXT, result TEXT, feedback TEXT, created_at REAL NOT NULL,
        updated_at REAL NOT NULL, finished_at REAL)""")
    conn.execute("""INSERT INTO cards(id,kind,lane,title,goal,acceptance,step,source,created_by,state,created_at,updated_at)
                    VALUES('legacy','task','task','[ok] old card','g','true',1,'s','secretary','created',1,1)""")
    conn.commit()
    conn.close()
    migrated = store.connect(db, owner_actor='owner')
    card = api.get(migrated, 'legacy')
    assert (card['assignee'], card['on_unavailable'], card['executor_wait']) == (None, 'wait', None)
    assert api.listing(migrated)[0]['assignee'] is None
    d = dispatcher.Dispatcher(dict(cfg, db=str(db)), migrated, limits_fetcher=lambda c: {})
    record_spawns(d)
    d.advance()
    d.launch()
    legacy = api.get(migrated, 'legacy')
    assert legacy['state'] == 'running' and len(workers(legacy)) == 1
    assert 'assignee_fallback' not in kinds(migrated, 'legacy') and 'executor_wait' not in kinds(migrated, 'legacy')


# ── model pin ──────────────────────────────────────────────────────────────

@pytest.mark.parametrize('pin, reason', [
    ({'effort': 'medium'}, '--effort requires --model'),
    ({'model': 'missing'}, 'unknown model'),
    ({'model': 'gpt-6-astra'}, 'specify --effort'),
    ({'model': 'gpt-6-luna', 'effort': 'impossible'}, 'effort'),
    ({'model': 'gpt-6-luna', 'assignee': 'claude'}, 'conflicts'),
    ({'model': 'gpt-6-luna', 'kind': 'epic'}, 'epic'),
])
def test_model_pin_validation_writes_nothing(env, pin, reason):
    _, conn, _ = env
    with pytest.raises(api.CardError, match=reason):
        mk(conn, '[ok] refused pin', **pin)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 0
    assert conn.execute('SELECT count(*) FROM events').fetchone()[0] == 0


def test_model_pin_resolves_loaded_config_and_step_effort(env):
    cfg, conn, _ = env
    cfg['ladder'][3]['codex'] = {'model': 'custom-worker', 'effort': 'custom-effort'}
    cid = mk(conn, '[ok] configured pin', step=3, model='custom-worker', cfg=cfg)
    card = api.get(conn, cid)
    assert (card['assignee'], card['model'], card['effort']) == ('codex', 'custom-worker', 'custom-effort')
    for step, effort in ((3, 'medium'), (4, 'high')):
        card = api.get(conn, mk(conn, '[ok] paired effort', step=step, model='claude-opus-5-5'))
        assert card['effort'] == effort
    cross = api.get(conn, mk(conn, '[ok] explicit effort', model='gpt-6-astra', effort='high'))
    assert (cross['assignee'], cross['effort']) == ('codex', 'high')


def test_model_pin_refuses_ambiguous_or_unrunnable_vendor(env):
    cfg, conn, _ = env
    cfg['ladder'][1]['dots'] = {'model': 'dots-model', 'effort': 'medium'}
    cfg['vendors']['dots'] = {}
    with pytest.raises(api.CardError, match='unknown executor'):
        mk(conn, '[ok] unrunnable pin', model='dots-model', cfg=cfg)
    cfg['ladder'][2]['claude']['model'] = cfg['ladder'][1]['codex']['model']
    with pytest.raises(api.CardError, match='ambiguous'):
        mk(conn, '[ok] ambiguous pin', model=cfg['ladder'][1]['codex']['model'], cfg=cfg)
    assert api.listing(conn) == []


def test_model_pin_cli_visibility_and_assignment(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] cli pin', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'system:test', '--created-by', 'secretary',
                    '--model', 'gpt-6-astra', '--effort', 'high') == 0
    cid = capsys.readouterr().out.split()[0]
    for card in (api.get(conn, cid), api.listing(conn)[0]):
        assert (card['assignee'], card['model'], card['effort']) == ('codex', 'gpt-6-astra', 'high')
    created = events_of(conn, cid, 'created')[0]['data']
    assert (created['assignee'], created['model'], created['effort']) == ('codex', 'gpt-6-astra', 'high')
    for command in ('show', 'list'):
        argv = (command, cid) if command == 'show' else (command,)
        assert hive_cli(monkeypatch, cfg, *argv) == 0
        output = capsys.readouterr().out
        assert 'gpt-6-astra' in output and 'high' in output and 'codex' in output
        assert hive_cli(monkeypatch, cfg, *argv, '--json') == 0
        output = json.loads(capsys.readouterr().out)
        card = output if command == 'show' else output[0]
        assert (card['assignee'], card['model'], card['effort']) == ('codex', 'gpt-6-astra', 'high')
    before = len(api.get(conn, cid)['events'])
    for assignee in ('claude', 'unknown', None):
        with pytest.raises(api.CardError, match='gpt-6-astra'):
            api.assign(conn, cid, assignee, executors=EXECUTORS)
    assert hive_cli(monkeypatch, cfg, 'assign', cid, 'claude') == 2
    assert 'gpt-6-astra' in capsys.readouterr().err
    assert len(api.get(conn, cid)['events']) == before
    assert api.assign(conn, cid, 'codex', on_unavailable='fallback', executors=EXECUTORS)
    assert api.get(conn, cid)['model'] == 'gpt-6-astra'


def test_model_pin_cli_refusals_and_help(env, monkeypatch, capsys):
    cfg, conn, _ = env
    base = ('create', '--title', 'pin', '--goal', 'g', '--acceptance', 'true', '--step', '1',
            '--source', 'system:test', '--created-by', 'secretary')
    for args, reason in ((('--effort', 'high'), '--effort requires --model'),
                         (('--model', 'gpt-6-astra'), 'specify --effort'),
                         (('--model', 'gpt-6-luna', '--assignee', 'claude'), 'conflicts')):
        assert hive_cli(monkeypatch, cfg, *base, *args) == 2
        assert reason in capsys.readouterr().err
    assert api.listing(conn) == []
    with pytest.raises(SystemExit) as exc:
        hive_cli(monkeypatch, cfg, 'create', '--help')
    assert exc.value.code == 0
    help_text = capsys.readouterr().out
    assert '--model' in help_text and '--effort' in help_text and 'ladder' in help_text


def test_model_pin_worker_and_reviewer_heads(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = mk(conn, '[round2] pinned retry', model='gpt-6-astra', effort='high', review=True)
    plain = mk(conn, '[ok] ladder choice')
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert tick_until_terminal(d, conn, plain) == 'done'
    card = api.get(conn, cid)
    assert [(r['round'], r['vendor'], r['model'], r['effort']) for r in workers(card)] == [
        (1, 'codex', 'gpt-6-astra', 'high'), (2, 'codex', 'gpt-6-astra', 'high')]
    reviewer = next(r for r in card['runs'] if r['role'] == 'reviewer')
    assert (reviewer['vendor'], reviewer['model'], reviewer['effort']) == ('claude', 'claude-sonnet-5-5', 'medium')
    calls = [json.loads(line) for line in (Path(card['workdir']) / 'calls.log').read_text().splitlines()]
    for call in (c for c in calls if not c['reviewer']):
        argv = call['argv']
        assert argv[argv.index('-m') + 1] == 'gpt-6-astra'
        assert 'model_reasoning_effort="high"' in argv
    for claim in (e['data'] for e in card['events'] if e['kind'] == 'claimed' and e['data']['role'] == 'worker'):
        assert (claim['vendor'], claim['model'], claim['effort']) == ('codex', 'gpt-6-astra', 'high')
    run = workers(api.get(conn, plain))[0]
    assert (run['model'], run['effort']) == (cfg['ladder'][1][run['vendor']]['model'], cfg['ladder'][1][run['vendor']]['effort'])
    assert api.get(conn, plain)['model'] is None


@pytest.mark.parametrize('unavailable', ['disabled', 'daily_runs', 'weekly', 'cooldown'])
def test_model_pin_waits_for_vendor_and_recovers_with_stored_head(env, unavailable):
    cfg, conn, d = env
    record_spawns(d)
    d.clock = lambda: 1000.0
    cid = mk(conn, '[ok] unavailable pin', step=3, model='gpt-6-astra', effort='high')
    limits = {}
    if unavailable == 'weekly':
        limits = {'codex': 0.0}
    elif unavailable == 'cooldown':
        store.meta_set(conn, 'provider_cooldown:codex', {'until': 1010.0, 'failures': 1})
    else:
        if unavailable == 'disabled':
            cfg['vendors']['codex']['disabled'] = True
        else:
            spend_daily(conn, cfg, 'codex')
    d.advance()
    assert d.start_run(api.card_row(conn, cid), limits) is None
    card = api.get(conn, cid)
    assert (card['state'], card['round'], card['runs']) == ('queued', 0, [])
    assert card['executor_wait']['executor'] == 'codex'
    assert kinds(conn, cid).count('executor_wait') == 1
    cfg['vendors']['codex'].update(disabled=False, daily_runs=100)
    d.clock = lambda: 1010.0
    cfg['ladder'][3]['codex'] = {'model': 'replacement', 'effort': 'medium'}
    assert d.start_run(api.card_row(conn, cid), {}) is not None
    run = workers(api.get(conn, cid))[0]
    assert (run['vendor'], run['model'], run['effort']) == ('codex', 'gpt-6-astra', 'high')


@pytest.mark.parametrize('policy', ['wait', 'fallback'])
def test_model_pin_provider_failure_obeys_policy(env, policy):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cfg['provider_cooldown'] = {'base_s': 10, 'max_s': 10}
    now = [1000.0]
    d.clock = lambda: now[0]
    cid = mk(conn, '[ok] [provider429] model outage', model='claude-opus-5-5', effort='high', on_unavailable=policy)
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    failed = workers(card)[0]
    assert (failed['model'], failed['effort'], failed['provider_failure']) == ('claude-opus-5-5', 'high', 'http_429')
    if policy == 'wait':
        assert card['state'] == 'queued' and len(workers(card)) == 1
        assert 'assignee_provider_wait' in kinds(conn, cid)
        assert card['executor_wait']['executor'] == 'claude'
    else:
        switched = workers(card)[1]
        event = events_of(conn, cid, 'assignee_fallback')[0]['data']
        assert (event['pinned_model'], event['actual_model']) == ('claude-opus-5-5', switched['model'])
        assert (switched['vendor'], switched['model']) == ('codex', 'gpt-6-luna')
        assert 'http_429' in event['reason']
    now[0] += 10
    assert tick_until_terminal(d, conn, cid) == 'done'
    if policy == 'wait':
        assert [(r['round'], r['model'], r['effort']) for r in workers(api.get(conn, cid))] == [
            (1, 'claude-opus-5-5', 'high'), (1, 'claude-opus-5-5', 'high')]


def test_model_pin_unavailable_fallback_is_journaled_and_retry_restores_pin(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    spend_daily(conn, cfg, 'codex')
    cid = mk(conn, '[round2] model fallback', model='gpt-6-astra', effort='high', on_unavailable='fallback')
    d.tick()
    card = api.get(conn, cid)
    run = workers(card)[0]
    assert (run['vendor'], run['model']) == ('claude', 'claude-sonnet-5-5')
    event = events_of(conn, cid, 'assignee_fallback')[0]['data']
    assert (event['pinned_model'], event['actual_model'], event['assigned'], event['actual']) == (
        'gpt-6-astra', 'claude-sonnet-5-5', 'codex', 'claude')
    cfg['vendors']['codex']['daily_runs'] = 100
    assert tick_until_terminal(d, conn, cid) == 'done'
    assert [(r['model'], r['effort']) for r in workers(api.get(conn, cid))] == [
        ('claude-sonnet-5-5', 'medium'), ('gpt-6-astra', 'high')]


def test_model_pin_recurring_instance_copies_resolved_pin(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    sid = mk_series(conn, every=60, max_runs=1, model='gpt-6-astra', effort='high', on_unavailable='fallback')
    d.tick()
    instance = api.get(conn, api.get(conn, sid)['instances'][0]['id'])
    assert (instance['assignee'], instance['model'], instance['effort'], instance['on_unavailable']) == (
        'codex', 'gpt-6-astra', 'high', 'fallback')
    assert (workers(instance)[0]['model'], workers(instance)[0]['effort']) == ('gpt-6-astra', 'high')


def test_model_pin_migration_preserves_existing_assignment(env, tmp_path):
    cfg, _, _ = env
    db = tmp_path / 'legacy-model.db'
    legacy = store.connect(db, owner_actor='owner')
    cid = mk(legacy, '[ok] legacy assignment', assignee='codex', executors=EXECUTORS)
    legacy.execute('ALTER TABLE cards DROP COLUMN model')
    legacy.execute('ALTER TABLE cards DROP COLUMN effort')
    legacy.close()
    migrated = store.connect(db, owner_actor='owner')
    card = api.get(migrated, cid)
    assert (card['assignee'], card['model'], card['effort']) == ('codex', None, None)
    assert api.listing(migrated)[0]['model'] is None
    d = dispatcher.Dispatcher(cfg, migrated, limits_fetcher=lambda c: {})
    record_spawns(d)
    d.advance()
    d.launch()
    run = workers(api.get(migrated, cid))[0]
    assert (run['model'], run['effort']) == ('gpt-6-luna', 'medium')


# ── recurring series ────────────────────────────────────────────────────────

def panel_card(conn, cfg, **kw):
    cid, _ = api.create(conn, title='[ok] independent work', goal='answer the same brief',
                        acceptance=kw.pop('acceptance', "grep -Eq 'Agreements:|sonnet' REPORT.md"), step=kw.pop('step', 1),
                        source='system:test', created_by='secretary', panel=kw.pop('panel', True), cfg=cfg, **kw)
    return cid


def finish_synthesis(pool, conn, d, cid, report=None):
    card = api.get(conn, cid)
    run = card['runs'][-1]
    assert run['member'] == store.SYNTHESIS
    if report is None:
        report = 'Agreements: ' + ', '.join(m['label'] for m in card['panel'])
    pool.claim(run['id'])
    Path(pool.job(run['id'])['cwd'], 'REPORT.md').write_text(report)
    pool.end(run['id'], 0, None, report, 'file')
    d.tick()


def test_panel_resolution_uses_loaded_ladder_and_persists(env):
    cfg, conn, _ = env
    cfg['ladder'][5]['claude'] = {'model': 'custom-opus', 'effort': 'custom-high'}
    cfg['ladder'][5]['codex'] = {'model': 'custom-astra', 'effort': 'custom-max'}
    cfg['ladder'][5]['unsupported'] = {'model': 'ignored', 'effort': 'high'}
    cid = panel_card(conn, cfg, step=5)
    members = api.get(conn, cid)['panel']
    assert members == [
        {'label': 'custom-opus', 'vendor': 'claude', 'model': 'custom-opus', 'effort': 'custom-high'},
        {'label': 'custom-astra', 'vendor': 'codex', 'model': 'custom-astra', 'effort': 'custom-max'}]
    assert api.listing(conn)[0]['panel'] == members
    assert events_of(conn, cid, 'created')[0]['data']['panel'] == members
    reopened = store.connect(cfg['db'])
    assert api.get(reopened, cid)['panel'] == members
    reopened.close()
    explicit = panel_card(conn, cfg, panel='custom-astra:medium,claude-opus-5-5', step=3)
    assert [(m['model'], m['effort']) for m in api.get(conn, explicit)['panel']] == [
        ('custom-astra', 'medium'), ('claude-opus-5-5', 'medium')]


@pytest.mark.parametrize('pins,labels', [
    ([('claude', 'claude-opus-5-5', 'high'), ('claude', 'claude-opus-5-5', 'medium'),
      ('codex', 'gpt-6.1-sol', 'medium')], ['opus-high', 'opus-medium', 'sol']),
    ([('claude', 'claude-opus-5-5', 'high'), ('claude', 'claude-opus-5-6', 'high'),
      ('codex', 'gpt-6-astra', 'high')], ['opus-high', 'opus-high-2', 'astra']),
    ([('claude', 'claude-opus-5-5', 'high'), ('codex', 'gpt-6-opus', 'high')],
     ['opus-high', 'opus-high-2']),
    ([('claude', 'claude-opus-5-5', 'high /'), ('claude', 'claude-opus-5-5', 'high!'),
      ('codex', 'gpt-6-astra', 'high')], ['opus-high', 'opus-high-2', 'astra']),
    ([('claude', 'claude-../../Opus 5.5', 'high'), ('codex', 'gpt-6/ASTRA', 'high')],
     ['opus', 'astra']),
    ([('claude', 'claude-opus-5-5', 'high'), ('claude', 'claude-opus-5-5', 'medium'),
      ('claude', 'claude-opus-high-5-5', 'high'), ('codex', 'gpt-6-astra', 'high')],
     ['opus-high-2', 'opus-medium', 'opus-high', 'astra']),
])
def test_panel_model_labels_are_file_safe_and_order_independent(env, pins, labels):
    cfg, conn, _ = env
    cfg['ladder'] = {step: {vendor: {'model': model, 'effort': effort}}
                     for step, (vendor, model, effort) in enumerate(pins, 1)}
    panel = [f'{model}:{effort}' for _, model, effort in pins]
    cid = panel_card(conn, cfg, panel=panel)
    members = api.get(conn, cid)['panel']
    assert [m['label'] for m in members] == labels
    assert len({m['label'] for m in members}) == len(members)
    assert all(m['label'] and set(m['label']) <= set('abcdefghijklmnopqrstuvwxyz0123456789-') for m in members)
    reversed_cid = panel_card(conn, cfg, panel=panel[::-1])
    assert api.get(conn, reversed_cid)['panel'] == members[::-1]


@pytest.mark.parametrize('options,reason', [
    ({'model': 'gpt-6-luna'}, 'panel'),
    ({'effort': 'high'}, 'panel'),
    ({'assignee': 'claude'}, 'panel'),
    ({'review': True}, 'panel'),
    ({'kind': 'epic'}, 'panel'),
    ({'kind': 'recurring', 'every': 60, 'max_runs': 1}, 'panel'),
    ({'panel': 'gpt-6-luna'}, 'two distinct'),
    ({'panel': 'gpt-6-luna,gpt-6-luna'}, 'two distinct'),
    ({'panel': 'gpt-6-luna,gpt-6-astra:high'}, 'two distinct'),
    ({'panel': 'gpt-6-luna,gpt-6-luna,claude-sonnet-5-5'}, 'two distinct'),
    ({'panel': 'missing,claude-sonnet-5-5'}, 'unknown model'),
    ({'panel': 'gpt-6-astra,claude-sonnet-5-5'}, 'specify --effort'),
    ({'panel': 'gpt-6-luna:impossible,claude-sonnet-5-5'}, 'effort'),
    ({'panel': 'gpt-6-luna:,claude-sonnet-5-5'}, 'effort'),
    ({'panel': 'gpt-6-luna,,claude-sonnet-5-5'}, 'members'),
    ({'panel': 'gpt-6-luna:high:extra,claude-sonnet-5-5'}, 'members'),
    ({'panel': False}, 'panel'),
])
def test_panel_create_refusals_write_nothing(env, options, reason):
    cfg, conn, _ = env
    before = counts(conn)
    with pytest.raises(api.CardError, match=reason):
        panel_card(conn, cfg, **options)
    assert counts(conn) == before
    assert not Path(cfg['workspaces']).exists()


def test_panel_refuses_unrunnable_ambiguous_and_single_vendor_defaults(env):
    cfg, conn, _ = env
    before = counts(conn)
    cfg['ladder'][1]['unsupported'] = {'model': 'other-model', 'effort': 'medium'}
    with pytest.raises(api.CardError, match='runnable'):
        panel_card(conn, cfg, panel='other-model,gpt-6-luna')
    cfg['ladder'][1]['unsupported']['model'] = 'gpt-6-luna'
    with pytest.raises(api.CardError, match='ambiguous'):
        panel_card(conn, cfg, panel='gpt-6-luna,claude-sonnet-5-5')
    del cfg['vendors']['codex']
    with pytest.raises(api.CardError, match='two distinct'):
        panel_card(conn, cfg)
    assert counts(conn) == before


def test_panel_cli_visibility_refusals_and_help(env, monkeypatch, capsys):
    cfg, conn, _ = env
    args = ('create', '--title', 'panel', '--goal', 'g', '--acceptance', 'true',
            '--step', '1', '--source', 'system:test', '--created-by', 'secretary')
    for extra in (('--panel', '--model', 'gpt-6-luna'), ('--panel', '--effort', 'high'),
                  ('--panel', '--assignee', 'claude'), ('--panel', '--review'), ('--panel', '--epic'),
                  ('--panel', '--every', '1m', '--max-runs', '1'), ('--panel', '--accept-review', 'criteria')):
        # Judge and command acceptance are mutually exclusive in the parser.
        actual = args[:5] + args[7:] if '--accept-review' in extra else args
        assert hive_cli(monkeypatch, cfg, *actual, *extra) == 2
        assert 'panel' in capsys.readouterr().err
    assert counts(conn) == (0, 0, 0, 0)
    for extra in (('--panel',), ('--panel', 'gpt-6-luna,claude-sonnet-5-5:high')):
        assert hive_cli(monkeypatch, cfg, *args, *extra) == 0
        capsys.readouterr()
    cid = api.listing(conn)[0]['id']
    panel = api.get(conn, cid)['panel']
    for command in (('show', cid), ('list',)):
        assert hive_cli(monkeypatch, cfg, *command) == 0
        output = capsys.readouterr().out
        assert 'panel' in output and all(m['model'] in output and m['label'] in output for m in panel)
        assert hive_cli(monkeypatch, cfg, *command, '--json') == 0
        data = json.loads(capsys.readouterr().out)
        assert (data if isinstance(data, dict) else data[0])['panel'] == panel
    before = counts(conn)
    assert hive_cli(monkeypatch, cfg, 'steer', cid, 'clarify') == 2
    assert 'panel' in capsys.readouterr().err
    with pytest.raises(api.CardError, match='panel'):
        api.assign(conn, cid, 'claude', executors=heads.executors(cfg))
    assert counts(conn) == before
    with pytest.raises(SystemExit) as e:
        hive_cli(monkeypatch, cfg, 'create', '--help')
    assert e.value.code == 0
    help_text = capsys.readouterr().out
    assert '--panel' in help_text and 'vendors' in help_text
    assert all(text in help_text for text in ('REPORT-opus.md', 'REPORT-astra.md', 'effort', 'suffix'))


def test_panel_slots_independence_and_restart(env, monkeypatch):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg, panel='claude-sonnet-5-5,gpt-6-luna,claude-opus-5-5:high')
    d.tick()
    first = workers(api.get(conn, cid))
    assert [r['member'] for r in first] == ['sonnet', 'luna']
    for r in first:
        pool.claim(r['id'])
    assert [r['status'] for r in workers(api.get(conn, cid))] == ['running', 'running']
    assert d.active_runs('task') == 2
    assert [heads.runs_today(conn, v, cfg['owner_tz']) for v in ('claude', 'codex')] == [1, 1]
    jobs = [pool.job(r['id']) for r in first]
    assert len({job['cwd'] for job in jobs}) == 2
    for job in jobs:
        assert Path(job['cwd']).is_relative_to(Path(cfg['workspaces']) / cid / 'members')
        assert 'answer the same brief' in job['prompt']
    # A new connection sees both claims, including runners still in starting.
    reopened = store.connect(cfg['db'])
    restarted = dispatcher.Dispatcher(cfg, reopened, limits_fetcher=lambda c: {})
    restarted.spawn = d.spawn
    restarted.tick()
    assert len(workers(api.get(conn, cid))) == 2
    pool.end(first[0]['id'], 0, None, 'private first report', 'file')
    restarted.tick()
    runs = workers(api.get(conn, cid))
    assert [r['member'] for r in runs] == ['sonnet', 'luna', 'opus']
    assert d.active_runs('task') == 2
    assert [heads.runs_today(conn, v, cfg['owner_tz']) for v in ('claude', 'codex')] == [2, 1]
    assert 'private first report' not in runs[-1]['prompt']
    assert len({pool.job(r['id'])['cwd'] for r in runs}) == 3
    assert Path(cfg['workspaces'], cid, 'REPORT-sonnet.md').read_text() == 'private first report'
    # Recovery after a report was handled keeps that member and its report.
    again = dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {})
    again.spawn = d.spawn
    again.tick()
    assert len(workers(api.get(conn, cid))) == 3
    for r in runs[1:]:
        if r['status'] == 'starting':
            pool.claim(r['id'])
        pool.end(r['id'], 0, None, f"answer from {r['member']}", 'file')
    again.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'running' and len(card['runs']) == 4
    synthesis = card['runs'][-1]
    assert synthesis['member'] == store.SYNTHESIS and not card['gates']
    for r in card['runs'][:-1]:
        assert r['report'] in synthesis['prompt'] and r['model'] in synthesis['prompt']
        assert r['effort'] in synthesis['prompt']
        assert f"members/r1/{r['member']}/" in synthesis['prompt']
        assert Path(card['workdir'], r['report_file']).read_text() == r['report']
    finish_synthesis(pool, conn, again, cid)
    card = api.get(conn, cid)
    assert card['state'] == 'done' and len(card['runs']) == 4
    assert card['gates'][0]['passed'] == 1
    assert Path(card['workdir'], 'REPORT.md').read_text() == card['runs'][-1]['report']
    reopened.close()


@pytest.mark.parametrize('unavailable', ['daily', 'weekly', 'cooldown', 'disabled'])
def test_panel_member_waits_without_substitution_and_recovers(env, monkeypatch, unavailable):
    cfg, conn, d = env
    InProcessRunners(conn, d, monkeypatch)
    limits = {'claude': 100, 'codex': 100}
    d.limits_fetcher = lambda c: limits
    if unavailable == 'daily':
        spend_daily(conn, cfg, 'codex')
    elif unavailable == 'weekly':
        limits['codex'] = 1
    elif unavailable == 'disabled':
        cfg['vendors']['codex']['disabled'] = True
    else:
        store.meta_set(conn, 'provider_cooldown:codex', {'until': time.time() + 3600, 'failures': 1})
    cid = panel_card(conn, cfg, step=3)
    d.tick()
    d.tick()
    assert [r['vendor'] for r in workers(api.get(conn, cid))] == ['claude']
    waits = events_of(conn, cid, 'panel_wait')
    assert len(waits) == 1 and waits[0]['data']['member'] == 'sol' and waits[0]['data']['reason']
    cfg['vendors']['codex']['disabled'] = True
    d.tick()
    expected_waits = 1 if unavailable == 'disabled' else 2
    assert len(events_of(conn, cid, 'panel_wait')) == expected_waits
    cfg['vendors']['codex'].update(daily_runs=100, disabled=False)
    limits['codex'] = 100
    store.meta_set(conn, 'limits', limits)
    conn.execute("DELETE FROM meta WHERE key='provider_cooldown:codex'")
    saved = api.get(conn, cid)['panel'][1]
    cfg['ladder'][3]['codex'] = {'model': 'replacement', 'effort': 'high'}
    d.tick()
    runs = workers(api.get(conn, cid))
    assert [(r['vendor'], r['model'], r['effort']) for r in runs] == [
        ('claude', 'claude-opus-5-5', 'medium'), (saved['vendor'], saved['model'], saved['effort'])]


@pytest.mark.parametrize('stage', ['members', 'synthesis'])
def test_panel_concurrent_claims_obey_slots(env, monkeypatch, stage):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg, panel=('claude-sonnet-5-5,gpt-6-luna,claude-opus-5-5:high'
                                      if stage == 'members' else True))
    d.advance()
    if stage == 'synthesis':
        d.launch()
        for run in api.get(conn, cid)['runs']:
            pool.claim(run['id'])
            pool.end(run['id'], 0, None, run['member'], 'file')
        d.advance()
    barrier = threading.Barrier(4)
    errors = []

    def launch():
        local = store.connect(cfg['db'])
        try:
            contender = dispatcher.Dispatcher(cfg, local, limits_fetcher=lambda c: {})
            contender.spawn = lambda rid, wd: None
            barrier.wait(timeout=2)
            contender.launch()
        except Exception as e:
            errors.append(e)
        finally:
            local.close()

    threads = [threading.Thread(target=launch) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout=3)
    assert not errors and not any(thread.is_alive() for thread in threads)
    runs = workers(api.get(conn, cid))
    expected = [('sonnet', 1), ('luna', 1)]
    if stage == 'synthesis':
        expected.append((store.SYNTHESIS, 1))
    assert [(r['member'], r['round']) for r in runs] == expected
    assert d.active_runs('task') == (1 if stage == 'synthesis' else cfg['concurrency'])


def test_panel_real_runner_synthesis_delivers_to_gate_workspace(env):
    cfg, conn, d = env
    run_heads_inline(cfg, d)
    cid = panel_card(conn, cfg, step=5,
                     acceptance="test -s out.txt && grep -q opus REPORT.md && grep -q astra REPORT.md")
    assert tick_until_terminal(d, conn, cid) == 'done'
    card = api.get(conn, cid)
    assert [(r['member'], r['report_file']) for r in card['runs']] == [
        ('opus', 'REPORT-opus.md'), ('astra', 'REPORT-astra.md'), (store.SYNTHESIS, 'REPORT.md')]
    for r in card['runs']:
        assert api.fresh_report(conn, r['id']) == r['report']
        assert Path(card['workdir'], r['report_file']).read_text() == r['report']
        expected = (Path(card['workdir']) if r['member'] == store.SYNTHESIS else
                    Path(card['workdir'], 'members', 'r1', r['member']))
        assert Path(r['run_dir']).parents[1] == expected
        assert (Path(r['run_dir']).parents[1] / r['report_file']).read_text() == r['report']
        assert (expected / 'out.txt').is_file()
    assert len(events_of(conn, cid, 'panel_synthesized')) == 1
    assert len(events_of(conn, cid, 'claimed')) == 3
    payload = json.loads(card['outbox'][0]['payload'])
    assert payload['report_excerpt'] == card['runs'][-1]['report']
    assert all(any(r['member'] in h and r['model'] in h and r['status'] in h for h in payload['heads'])
               for r in card['runs'])


@pytest.mark.parametrize('failure', ['omitted', 'substring', 'nonzero', 'crashed', 'timeout',
                                    'failed_launch', 'missing', 'empty', 'fallback', 'provider'])
def test_panel_synthesis_failure_retries_alone(env, monkeypatch, failure):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg)
    d.tick()
    for run in api.get(conn, cid)['runs']:
        pool.claim(run['id'])
        pool.end(run['id'], 0, None, f"answer from {run['member']}", 'file')
    d.tick()
    synthesis = api.get(conn, cid)['runs'][-1]
    assert synthesis['member'] == store.SYNTHESIS
    pool.claim(synthesis['id'])
    report = {'omitted': 'sonnet alone', 'substring': 'sonnet luna-extra',
              'missing': None, 'empty': '   '}.get(failure, 'sonnet luna')
    pool.end(synthesis['id'], 1 if failure == 'nonzero' else 0, None, report,
             'fallback' if failure == 'fallback' else 'none' if failure == 'missing' else 'file')
    if failure in ('crashed', 'timeout', 'failed_launch'):
        conn.execute('UPDATE runs SET status=? WHERE id=?', (failure, synthesis['id']))
    if failure == 'provider':
        conn.execute("UPDATE runs SET provider_failure='http_429' WHERE id=?", (synthesis['id'],))
    d.tick()
    card = api.get(conn, cid)
    assert not card['gates'] and len(events_of(conn, cid, 'retry')) == 1
    assert [(r['member'], r['round']) for r in card['runs']] == [
        ('sonnet', 1), ('luna', 1), (store.SYNTHESIS, 1), (store.SYNTHESIS, 1)]
    for member in card['panel']:
        assert Path(card['workdir'], f"REPORT-{member['label']}.md").read_text() == f"answer from {member['label']}"
    finish_synthesis(pool, conn, d, cid)
    assert api.get(conn, cid)['state'] == 'done'


def test_panel_synthesis_failure_budget_blocks_once(env, monkeypatch):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg)
    d.tick()
    for run in api.get(conn, cid)['runs']:
        pool.claim(run['id'])
        pool.end(run['id'], 0, None, run['member'], 'file')
    d.tick()
    finish_synthesis(pool, conn, d, cid, 'sonnet alone')
    finish_synthesis(pool, conn, d, cid, 'sonnet alone')
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and len(card['runs']) == 4
    assert len(card['outbox']) == len(events_of(conn, cid, 'blocked')) == 1
    assert not card['gates']


@pytest.mark.parametrize('wait', ['slot', 'daily', 'weekly', 'cooldown', 'disabled', 'ladder'])
def test_panel_synthesis_waits_for_admission(env, monkeypatch, wait):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg, step=5)
    d.tick()
    for run in api.get(conn, cid)['runs']:
        pool.claim(run['id'])
        pool.end(run['id'], 0, None, run['member'], 'file')
    d.advance()
    if wait == 'slot':
        busy = mk(conn, '[ok] busy', priority=100)
        cfg['concurrency'] = 1
    elif wait == 'weekly':
        store.meta_set(conn, 'limits', {'claude': 1, 'codex': 1})
    elif wait == 'ladder':
        ladder = cfg['ladder'][5]
        cfg['ladder'][5] = {}
    else:
        for vendor in cfg['vendors']:
            if wait == 'daily':
                spend_daily(conn, cfg, vendor)
            elif wait == 'disabled':
                cfg['vendors'][vendor]['disabled'] = True
            else:
                store.meta_set(conn, f'provider_cooldown:{vendor}', {'until': time.time() + 3600, 'failures': 1})
    d.tick()
    d.tick()
    assert len(api.get(conn, cid)['runs']) == 2 and not api.get(conn, cid)['gates']
    if wait == 'slot':
        run = api.get(conn, busy)['runs'][0]
        pool.claim(run['id'])
        Path(pool.job(run['id'])['cwd'], 'out.txt').write_text('42')
        pool.end(run['id'], 0, None, 'done', 'file')
    else:
        waits = events_of(conn, cid, 'budget_wait')
        assert len(waits) == 1 and waits[0]['data']['member'] == store.SYNTHESIS
        for vendor in cfg['vendors']:
            cfg['vendors'][vendor].update(daily_runs=100, disabled=False)
            conn.execute('DELETE FROM meta WHERE key=?', (f'provider_cooldown:{vendor}',))
        store.meta_set(conn, 'limits', {})
        if wait == 'ladder':
            cfg['ladder'][5] = ladder
    d.tick()
    card = api.get(conn, cid)
    synthesis = card['runs'][-1]
    assert synthesis['member'] == store.SYNTHESIS and len(card['runs']) == 3
    assert {k: synthesis[k] for k in ('vendor', 'model', 'effort')} == heads.head_for(cfg, 5, synthesis['vendor'])
    assert d.active_runs('task') == 1
    spent = len(cfg['vendors']) if wait == 'daily' else 0  # the runs spend_daily put each vendor at its ceiling with
    assert sum(heads.runs_today(conn, v, cfg['owner_tz']) for v in cfg['vendors']) == (4 if wait == 'slot' else 3) + spent


@pytest.mark.parametrize('phase', ['before', 'starting', 'running', 'reported', 'handled'])
def test_panel_synthesis_restart_preserves_single_claim(env, monkeypatch, capsys, phase):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg)
    d.tick()
    for run in api.get(conn, cid)['runs']:
        pool.claim(run['id'])
        pool.end(run['id'], 0, None, run['member'], 'file')
    if phase != 'before':
        d.tick()
        synthesis = api.get(conn, cid)['runs'][-1]
        if phase in ('running', 'reported', 'handled'):
            pool.claim(synthesis['id'])
        if phase in ('reported', 'handled'):
            Path(pool.job(synthesis['id'])['cwd'], 'REPORT.md').write_text('sonnet luna')
            pool.end(synthesis['id'], 0, None, 'sonnet luna', 'file')
        if phase == 'handled':
            d.advance()
    reopened = store.connect(cfg['db'])
    restarted = dispatcher.Dispatcher(cfg, reopened, limits_fetcher=lambda c: {})
    restarted.spawn = d.spawn
    restarted.tick()
    restarted.tick()
    card = api.get(reopened, cid)
    assert len(card['runs']) == 3
    assert [e['data']['member'] for e in events_of(reopened, cid, 'claimed')] == ['sonnet', 'luna', store.SYNTHESIS]
    for member in card['panel']:
        assert Path(card['workdir'], f"REPORT-{member['label']}.md").read_text() == member['label']
    if phase not in ('reported', 'handled'):
        if phase == 'running':
            pool.end(card['runs'][-1]['id'], 0, None, 'sonnet luna', 'file')
            Path(card['workdir'], 'REPORT.md').write_text('sonnet luna')
            restarted.tick()
        else:
            finish_synthesis(pool, conn, restarted, cid)
    assert api.get(reopened, cid)['state'] == 'done'
    assert len(api.get(reopened, cid)['gates']) == 1
    from scripts import hive as cli
    assert cli.main(['show', cid]) == 0
    output = capsys.readouterr().out
    assert store.SYNTHESIS in output and card['runs'][-1]['model'] in output
    reopened.close()


@pytest.mark.parametrize('failure', ['nonzero', 'crashed', 'timeout', 'failed_launch', 'missing', 'empty', 'fallback', 'provider'])
def test_panel_failure_retries_only_failed_member(env, monkeypatch, failure):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg)
    d.tick()
    a, b = workers(api.get(conn, cid))
    for r in (a, b):
        pool.claim(r['id'])
    pool.end(a['id'], 0, None, 'keep this report', 'file')
    code = 0 if failure in ('missing', 'empty', 'fallback') else 1
    report = None if failure == 'missing' else '   ' if failure == 'empty' else 'failed answer'
    pool.end(b['id'], code, 'synthetic failure', report,
             'fallback' if failure == 'fallback' else 'file')
    if failure in ('crashed', 'timeout', 'failed_launch'):
        conn.execute('UPDATE runs SET status=? WHERE id=?', (failure, b['id']))
    if failure == 'provider':
        conn.execute("UPDATE runs SET provider_failure='http_429' WHERE id=?", (b['id'],))
    d.tick()
    assert len(events_of(conn, cid, 'retry')) == 1
    if failure == 'provider':
        assert len(workers(api.get(conn, cid))) == 2
        assert events_of(conn, cid, 'panel_wait')[0]['data']['member'] == 'luna'
        conn.execute("DELETE FROM meta WHERE key='provider_cooldown:codex'")
        d.tick()
    runs = workers(api.get(conn, cid))
    assert [(r['member'], r['round']) for r in runs] == [('sonnet', 1), ('luna', 1), ('luna', 1)]
    assert (runs[-1]['model'], runs[-1]['effort']) == (b['model'], b['effort'])
    assert 'keep this report' not in runs[-1]['prompt']
    pool.claim(runs[-1]['id'])
    pool.end(runs[-1]['id'], 0, None, 'recovered answer', 'file')
    d.tick()
    synthesis = api.get(conn, cid)['runs'][-1]
    assert 'keep this report' in synthesis['prompt']
    finish_synthesis(pool, conn, d, cid)
    card = api.get(conn, cid)
    assert card['state'] == 'done'
    assert Path(card['workdir'], 'REPORT-sonnet.md').read_text() == 'keep this report'


def test_panel_gate_failure_retries_all_with_feedback(env, monkeypatch):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg, acceptance="grep -q 'second round' REPORT.md || { echo fix-the-answer; exit 1; }")
    d.tick()
    for r in workers(api.get(conn, cid)):
        pool.claim(r['id'])
        pool.end(r['id'], 0, None, 'first answer', 'file')
    d.tick()
    finish_synthesis(pool, conn, d, cid, 'sonnet luna first answer')
    card = api.get(conn, cid)
    assert card['state'] == 'running' and card['round'] == 2 and card['gates'][0]['passed'] == 0
    d.tick()
    card = api.get(conn, cid)
    assert [(r['member'], r['round']) for r in card['runs']] == [
        ('sonnet', 1), ('luna', 1), (store.SYNTHESIS, 1), ('sonnet', 2), ('luna', 2)]
    for r in card['runs'][3:]:
        assert 'fix-the-answer' in r['prompt']
        pool.claim(r['id'])
        pool.end(r['id'], 0, None, 'second round answer', 'file')
    d.tick()
    assert 'fix-the-answer' in api.get(conn, cid)['runs'][-1]['prompt']
    finish_synthesis(pool, conn, d, cid, 'sonnet luna second round answer')
    card = api.get(conn, cid)
    assert card['state'] == 'done' and [g['passed'] for g in card['gates']] == [0, 1]


def test_panel_failure_budget_exhaustion_blocks_once(env, monkeypatch):
    cfg, conn, d = env
    pool = InProcessRunners(conn, d, monkeypatch)
    cid = panel_card(conn, cfg)
    d.tick()
    a, b = workers(api.get(conn, cid))
    for r in (a, b):
        pool.claim(r['id'])
    pool.end(a['id'], 0, None, 'good answer', 'file')
    pool.end(b['id'], 1, 'failed', '', 'none')
    d.tick()
    retry = workers(api.get(conn, cid))[-1]
    pool.claim(retry['id'])
    pool.end(retry['id'], 1, 'failed again', '', 'none')
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and len(card['runs']) == 3
    assert len(card['outbox']) == 1 and 'panel luna' in card['block_reason']


def test_panel_additive_migration_preserves_plain_active_run(env, tmp_path):
    cfg, conn, d = env
    cid = mk(conn, '[ok] plain')
    d.spawn = lambda rid, wd: None
    d.tick()
    conn.close()
    legacy = sqlite3.connect(cfg['db'])
    legacy.execute('DROP INDEX runs_member_active_claim')
    legacy.execute('ALTER TABLE cards DROP COLUMN panel')
    legacy.execute('ALTER TABLE runs DROP COLUMN member')
    legacy.execute('ALTER TABLE runs DROP COLUMN report_file')
    legacy.execute("CREATE UNIQUE INDEX runs_active_claim ON runs(card_id,round,role) WHERE status IN ('starting','running')")
    legacy.commit()
    legacy.close()
    migrated = store.connect(cfg['db'])
    card = api.get(migrated, cid)
    assert card['panel'] is None and len(card['runs']) == 1
    restarted = dispatcher.Dispatcher(cfg, migrated, limits_fetcher=lambda c: {})
    restarted.spawn = lambda rid, wd: None
    restarted.tick()
    assert len(api.get(migrated, cid)['runs']) == 1
    panel = panel_card(migrated, cfg)
    cfg['concurrency'] = 3
    restarted.tick()
    assert [r['member'] for r in api.get(migrated, panel)['runs']] == ['sonnet', 'luna']
    migrated.close()


def mk_series(conn, title='[ok] periodic', **kw):
    cid, _ = api.create(conn, title=title, goal='make out.txt', acceptance='test -s out.txt', step=1,
                        source='system:test', created_by='secretary', kind='recurring', **kw)
    return cid


def fake_time(monkeypatch, d, start=1000.0):
    now = [start]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    d.clock = lambda: now[0]
    return now


def instance_ids(conn, sid):
    return [i['id'] for i in api.get(conn, sid)['instances']]


def test_delayed_runner_start_keeps_one_claim(env, monkeypatch):
    _, conn, d = env
    spawn = d.spawn
    monkeypatch.setattr(d, 'spawn', lambda *_: None)
    cid = mk(conn, '[ok] delayed interpreter startup')
    d.tick()
    run = api.get(conn, cid)['runs'][0]
    # A cold interpreter may take more than a second on the shared host.
    # Age the claim without sleeping, then launch the real runner after a tick.
    conn.execute('UPDATE runs SET started_at=? WHERE id=?', (time.time() - 2, run['id']))
    d.tick()
    c = api.get(conn, cid)
    assert len(c['runs']) == 1 and c['runs'][0]['status'] == 'starting'
    monkeypatch.setattr(d, 'spawn', spawn)
    spawn(run['id'], c['workdir'])
    assert run_until(d, conn, cid) == 'done'
    assert len(api.get(conn, cid)['runs']) == 1
    assert 'run_reaped' not in kinds(conn, cid)


def test_series_runs_each_period_through_heads_gate_and_keeps_history(env):
    _, conn, d = env
    offset = [0]
    d.clock = lambda: time.time() + offset[0]
    sid = mk_series(conn, every=60, max_runs=2, owner_visible=True)
    d.tick()
    first = instance_ids(conn, sid)
    assert len(first) == 1
    assert run_until(d, conn, first[0]) == 'done'
    offset[0] = 60
    d.tick()
    ids = instance_ids(conn, sid)
    assert len(ids) == 2
    assert run_until(d, conn, ids[1]) == 'done'
    d.tick()
    series = api.get(conn, sid)
    assert [(i['period'], i['state'], i['result']) for i in series['instances']] == [
        (1, 'done', 'judge done'), (2, 'done', 'judge done')]
    assert series['state'] == 'done' and series['finished_at'] is not None and series['wait_reason'] is None
    assert [o['kind'] for o in series['outbox']] == ['task_result']
    assert series['runs'] == [] and series['round'] == 0
    for period, iid in enumerate(ids, 1):
        inst = api.get(conn, iid)
        assert inst['card_class'] == 'series_run' and inst['owner_visible'] == 1
        assert inst['kind'] == 'task' and inst['series'] == {'id': sid, 'period': period}
        assert len(workers(inst)) == 1 and inst['gates'][0]['passed'] == 1


def test_open_series_waits_on_schedule_without_heads_rounds_or_outbox(env, monkeypatch):
    _, conn, d = env
    spawned = record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=3, not_before=1300.0)
    for t in (1000, 1100, 1299):
        now[0] = t
        d.tick()
    series = api.get(conn, sid)
    assert (series['state'], series['wait_reason'], series['round'], series['finished_at']) == ('blocked', 'schedule', 0, None)
    assert series['block_reasons'] == [{'kind': 'schedule'}]
    assert series['instances'] == [] and series['outbox'] == [] and series['runs'] == [] and spawned == []
    assert series['schedule'] == {'every': 60.0, 'max_runs': 3, 'until': None, 'overlap': 'skip', 'start': 1300.0,
                                  'stopped': False, 'stopped_at': None, 'next_start': 1300.0, 'created': 0, 'skipped': 0}
    assert not {'queued', 'running'} & {e['kind'] for e in series['events']}


def test_instance_inherits_assignment_and_period_start(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=2, assignee='codex', on_unavailable='fallback', executors=EXECUTORS,
                    review=True, priority=3)
    now[0] = 1075
    d.advance()
    d.launch()
    inst = api.get(conn, instance_ids(conn, sid)[0])
    assert (inst['assignee'], inst['on_unavailable'], inst['review'], inst['priority']) == ('codex', 'fallback', 1, 3)
    assert inst['not_before'] == 1060.0 and inst['series'] == {'id': sid, 'period': 2}
    assert inst['state'] == 'running' and worker_vendors(conn, inst['id']) == ['codex']


@pytest.mark.parametrize('limit', [{'max_runs': 2}, {'until': 1100.0}])
def test_series_stops_creating_at_its_limit_and_finishes_after_last_instance(env, monkeypatch, limit):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, **limit)
    for t in (1000, 1060, 1120, 1180):
        now[0] = t
        for iid in instance_ids(conn, sid):
            settle(conn, iid, 'done')
        d.advance()
    series = api.get(conn, sid)
    assert [i['period'] for i in series['instances']] == [1, 2]
    assert series['state'] == 'done' and series['schedule']['next_start'] is None
    assert series['result'] == 'series complete: 2 instance(s), 0 skipped'
    assert [o['kind'] for o in series['outbox']] == ['task_result']


def test_series_stays_open_until_last_instance_is_final(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=1)
    d.advance()
    now[0] = 1500
    d.advance()
    assert api.get(conn, sid)['state'] == 'blocked'
    settle(conn, instance_ids(conn, sid)[0], 'done')
    d.advance()
    assert api.get(conn, sid)['state'] == 'done'


def test_stop_ends_new_instances_keeps_existing_and_is_idempotent(env, monkeypatch, capsys):
    cfg, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=5)
    d.advance()
    running = instance_ids(conn, sid)[0]
    assert api.stop(conn, sid, actor='owner')
    assert hive_cli(monkeypatch, cfg, 'stop', sid) == 0
    assert 'unchanged' in capsys.readouterr().out
    now[0] = 1130
    d.advance()
    series = api.get(conn, sid)
    assert instance_ids(conn, sid) == [running] and api.get(conn, running)['state'] == 'queued'
    assert series['state'] == 'blocked' and series['schedule']['stopped'] and series['schedule']['next_start'] is None
    assert kinds(conn, sid).count('series_stopped') == 1
    settle(conn, running, 'done')
    d.advance()
    assert api.get(conn, sid)['state'] == 'done' and api.get(conn, sid)['result'].startswith('series stopped')
    assert not api.stop(conn, sid)


def test_stop_cli_stops_and_rejects_a_plain_card(env, monkeypatch, capsys):
    cfg, conn, d = env
    sid = mk_series(conn, every=60, max_runs=5, not_before=time.time() + 3600)
    assert hive_cli(monkeypatch, cfg, 'stop', sid) == 0
    assert f'{sid} stopped' in capsys.readouterr().out
    d.tick()
    assert api.get(conn, sid)['state'] == 'done' and api.get(conn, sid)['instances'] == []
    plain = mk(conn, '[ok] plain', not_before=time.time() + 3600)
    assert hive_cli(monkeypatch, cfg, 'stop', plain) == 2
    assert 'series_stopped' not in kinds(conn, plain)


def test_overlap_skip_skips_period_with_event_and_resumes(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=3)
    d.advance()
    first = instance_ids(conn, sid)[0]
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (first,))
    now[0] = 1060
    d.advance()
    d.advance()
    assert instance_ids(conn, sid) == [first]
    skipped = [e['data'] for e in api.get(conn, sid)['events'] if e['kind'] == 'periods_skipped']
    assert skipped == [{'first': 2, 'last': 2, 'count': 1, 'reason': 'overlap', 'running': first}]
    settle(conn, first, 'done')
    now[0] = 1120
    d.advance()
    series = api.get(conn, sid)
    assert [i['period'] for i in series['instances']] == [1, 3]
    assert (series['schedule']['created'], series['schedule']['skipped']) == (2, 1)


def test_overlap_queue_waits_for_previous_and_survives_its_failure(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=3, overlap='queue')
    d.advance()
    first = instance_ids(conn, sid)[0]
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (first,))
    now[0] = 1060
    d.advance()
    second = instance_ids(conn, sid)[1]
    card = api.get(conn, second)
    assert card['state'] == 'blocked' and card['block_reasons'] == [{'kind': 'dependency', 'blockers': [first]}]
    settle(conn, first, 'done')
    d.advance()
    assert api.get(conn, second)['state'] == 'queued'
    conn.execute("UPDATE cards SET state='running' WHERE id=?", (second,))
    now[0] = 1120
    d.advance()
    third = instance_ids(conn, sid)[2]
    assert api.get(conn, third)['blocked_by'] == [{'id': second, 'state': 'running'}]
    settle(conn, second, 'blocked', 'gate exit 1')
    d.advance()
    d.advance()
    assert api.get(conn, third)['state'] == 'queued'
    assert api.get(conn, sid)['state'] == 'blocked'
    assert [i['block_reason'] for i in api.get(conn, sid)['instances']] == [None, 'gate exit 1', None]


def test_downtime_creates_one_instance_for_latest_period_and_counts_missed(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=10)
    d.advance()
    settle(conn, instance_ids(conn, sid)[0], 'done')
    now[0] = 1250
    d.advance()
    series = api.get(conn, sid)
    assert [i['period'] for i in series['instances']] == [1, 5]
    assert api.get(conn, series['instances'][1]['id'])['not_before'] == 1240.0
    assert [e['data'] for e in series['events'] if e['kind'] == 'periods_skipped'] == [
        {'first': 2, 'last': 4, 'count': 3, 'reason': 'missed'}]
    assert (series['schedule']['skipped'], series['schedule']['next_start']) == (3, 1300.0)


def test_repeated_tick_and_new_dispatcher_create_no_duplicate(env, monkeypatch):
    cfg, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=3)
    now[0] = 1010
    d.tick()
    d.tick()
    reopened = store.connect(cfg['db'])
    fresh = dispatcher.Dispatcher(cfg, reopened, limits_fetcher=lambda c: {}, clock=lambda: now[0])
    record_spawns(fresh)
    fresh.tick()
    assert len(api.get(reopened, sid)['instances']) == 1
    assert conn.execute("SELECT count(*) FROM cards WHERE idem_key LIKE 'series:%'").fetchone()[0] == 1
    assert kinds(conn, sid).count('instance_created') == 1


def test_failed_instance_does_not_stop_series(env, monkeypatch):
    _, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, every=60, max_runs=2)
    d.advance()
    settle(conn, instance_ids(conn, sid)[0], 'blocked', 'gate exit 1')
    now[0] = 1060
    d.advance()
    series = api.get(conn, sid)
    assert [(i['period'], i['state']) for i in series['instances']] == [(1, 'blocked'), (2, 'queued')]
    assert series['state'] == 'blocked' and series['wait_reason'] == 'schedule'


@pytest.mark.parametrize('kw', [
    {'every': 0, 'max_runs': 1},
    {'every': -60, 'max_runs': 1},
    {'every': 60},
    {'every': 60, 'max_runs': 0},
    {'every': 60, 'until': 999.0},
    {'every': 60, 'until': 1200.0, 'not_before': 1200.0},
    {'every': 60, 'max_runs': 1, 'overlap': 'parallel'},
    {'every': 60, 'max_runs': 1, 'recheck_every': 60, 'recheck_max': 1},
    {'every': 60, 'max_runs': 1, 'blocked_by': ['other']},
    {'every': 60, 'max_runs': 1, 'parent': 'epic'},
    {'kind': 'epic', 'every': 60, 'max_runs': 1},
    {'kind': 'task', 'max_runs': 1},
])
def test_recurring_create_rejections_write_nothing(env, monkeypatch, kw):
    _, conn, _ = env
    monkeypatch.setattr(api.time, 'time', lambda: 1000.0)
    kw = dict(kw)
    kind = kw.pop('kind', 'recurring')
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(api.CardError):
        api.create(conn, title='x', goal='g', acceptance='true', step=1, source='system:test', created_by='secretary',
                   kind=kind, **kw)
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def test_create_cli_recurring_forwards_schedule_and_rejects(env, monkeypatch, capsys):
    cfg, conn, _ = env
    monkeypatch.setattr(api.time, 'time', lambda: 1700000000)
    base = ('create', '--title', 'periodic', '--goal', 'g', '--acceptance', 'true', '--step', '1',
            '--source', 'system:test', '--created-by', 'secretary')
    assert hive_cli(monkeypatch, cfg, *base, '--every', '1h', '--max-runs', '3', '--until', '2030-01-01T00:00:00+00:00',
                    '--overlap', 'queue', '--assignee', 'codex') == 0
    card = api.get(conn, capsys.readouterr().out.split()[0])
    assert card['kind'] == 'recurring' and card['assignee'] == 'codex'
    assert {k: card['schedule'][k] for k in ('every', 'max_runs', 'until', 'overlap', 'start')} == {
        'every': 3600.0, 'max_runs': 3, 'until': 1893456000.0, 'overlap': 'queue', 'start': 1700000000}
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    with pytest.raises(SystemExit) as exc:
        hive_cli(monkeypatch, cfg, *base, '--every', '0s', '--max-runs', '1')
    assert exc.value.code == 2
    for extra in (('--every', '1h'), ('--every', '1h', '--until', '2020-01-01T00:00:00+00:00'),
                  ('--every', '1h', '--max-runs', '1', '--recheck-every', '1h', '--recheck-max', '1'),
                  ('--every', '1h', '--max-runs', '1', '--epic'),
                  ('--every', '1h', '--max-runs', '1', '--parent', 'x'),
                  ('--every', '1h', '--max-runs', '1', '--blocked-by', 'x'),
                  ('--max-runs', '1',)):
        assert hive_cli(monkeypatch, cfg, *base, *extra) == 2
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def test_series_show_and_list_print_schedule_history_and_marks(env, monkeypatch, capsys):
    cfg, conn, d = env
    record_spawns(d)
    now = fake_time(monkeypatch, d)
    sid = mk_series(conn, title='[ok] nightly', every=60, max_runs=3)
    d.advance()
    first = instance_ids(conn, sid)[0]
    settle(conn, first, 'blocked', 'gate exit 1')
    assert hive_cli(monkeypatch, cfg, 'show', sid) == 0
    out = capsys.readouterr().out
    assert 'every 60s · max runs 3 · until — · overlap skip' in out
    assert 'created 1 · skipped 0' in out
    assert f'instance #1 {first} [blocked] gate exit 1' in out
    assert hive_cli(monkeypatch, cfg, 'show', first) == 0
    assert f'series: {sid} period #1' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'list') == 0
    lines = capsys.readouterr().out.splitlines()
    assert '[recurring] [ok] nightly' in next(line for line in lines if line.startswith(sid))
    assert f'(series {sid} #1)' in next(line for line in lines if line.startswith(first))
    rows = {r['id']: r for r in api.listing(conn)}
    assert (rows[sid]['kind'], rows[first]['series'], rows[first]['series_period']) == ('recurring', sid, 1)


def test_series_refuses_release_steer_dependency_and_epic_child(env):
    _, conn, _ = env
    sid = mk_series(conn, every=60, max_runs=1, not_before=time.time() + 3600)
    other = mk(conn, '[ok] other', not_before=time.time() + 3600)
    epic = mk_epic(conn, '[ok] epic')
    for call in (lambda: api.release(conn, sid), lambda: api.steer(conn, sid, 'do it', 'owner'),
                 lambda: api.add_blocker(conn, sid, other), lambda: api.add_child(conn, sid, epic)):
        with pytest.raises(api.CardError):
            call()
    assert kinds(conn, sid) == ['created', 'schedule_wait']


def test_due_recheck_creates_no_instance_or_work(env, monkeypatch, tmp_path):
    _, conn, d = env
    now = fake_time(monkeypatch, d)
    cid = completed_scheduled_card(conn, tmp_path, recheck_every=60, recheck_max=3)
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    now[0] = 1060
    assert d.advance() == 1
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before
    card = api.get(conn, cid)
    assert card['state'] == 'done' and card['round'] == 1 and card['runs'] == [] and card['instances'] is None


def test_legacy_cards_migrate_without_series_and_keep_recheck(env, tmp_path, monkeypatch):
    _, _, d = env
    db = tmp_path / 'legacy-series.db'
    conn = sqlite3.connect(db)
    conn.execute("""CREATE TABLE cards(
        id TEXT PRIMARY KEY, kind TEXT NOT NULL DEFAULT 'task', lane TEXT NOT NULL DEFAULT 'task',
        title TEXT NOT NULL, goal TEXT NOT NULL, acceptance TEXT NOT NULL, body TEXT NOT NULL DEFAULT '',
        step INTEGER NOT NULL, review INTEGER NOT NULL DEFAULT 0, task_type TEXT NOT NULL DEFAULT 'general',
        source TEXT NOT NULL, created_by TEXT NOT NULL, idem_key TEXT UNIQUE, priority INTEGER NOT NULL DEFAULT 0,
        not_before REAL, recheck_every REAL, recheck_until REAL, recheck_max INTEGER, recheck_due REAL,
        recheck_attempts INTEGER NOT NULL DEFAULT 0, state TEXT NOT NULL, round INTEGER NOT NULL DEFAULT 0,
        version INTEGER NOT NULL DEFAULT 0, workdir TEXT, block_reason TEXT, wait_reason TEXT, result TEXT,
        feedback TEXT, created_at REAL NOT NULL, updated_at REAL NOT NULL, finished_at REAL)""")
    conn.execute("""INSERT INTO cards(id,title,goal,acceptance,step,source,created_by,state,round,recheck_every,
                    recheck_max,recheck_due,recheck_attempts,created_at,updated_at)
                    VALUES('legacy','old','g','true',1,'s','secretary','done',1,60,3,1060,1,1,1)""")
    conn.commit()
    conn.close()
    migrated = store.connect(db, owner_actor='owner')
    card = api.get(migrated, 'legacy')
    assert (card['kind'], card['state'], card['round']) == ('task', 'done', 1)
    assert card['recheck_policy'] == {'every': 60.0, 'until': None, 'max': 3}
    assert card['recheck_state'] == {'due': 1060.0, 'attempts': 1}
    assert (card['schedule'], card['instances'], card['series']) == (None, None, None)
    assert api.listing(migrated)[0]['series'] is None
    monkeypatch.setattr(api.time, 'time', lambda: 1000.0)
    sid = mk_series(migrated, every=60, max_runs=1)
    assert api.get(store.connect(db, owner_actor='owner'), sid)['schedule']['next_start'] == 1000.0


# ── external executor dots ─────────────────────────────────────────────────

class Clock:
    """Real time plus a test-controlled offset, so polling intervals elapse without sleeping."""
    def __init__(self):
        self.offset = 0

    def __call__(self):
        return time.time() + self.offset


def dots_dispatcher(cfg, conn, clock):
    return dispatcher.Dispatcher(cfg, conn, limits_fetcher=lambda c: {}, clock=clock)


def dots_state(cfg):
    return Path(cfg['db']).parent / 'dots'


def dots_calls(cfg):
    path = dots_state(cfg) / 'calls.jsonl'
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def dots_reply(cfg, text, files=()):
    dots_state(cfg).mkdir(exist_ok=True)
    (dots_state(cfg) / 'reply.json').write_text(json.dumps({'text': text, 'files': [str(f) for f in files]}))


def dots_card(conn, cfg, title='dots card', **kw):
    return mk(conn, title, executor='dots', cfg=cfg, **kw)


def test_dots_executor_is_stored_shown_and_refused_before_write(env, monkeypatch, capsys):
    cfg, conn, _ = env
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'research', '--goal', 'g', '--acceptance', 'true',
                    '--step', '2', '--source', 'system:test', '--created-by', 'secretary', '--executor', 'dots') == 0
    cid = capsys.readouterr().out.split()[0]
    assert api.get(conn, cid)['executor'] == 'dots' and api.listing(conn)[0]['executor'] == 'dots'
    assert events_of(conn, cid, 'created')[0]['data']['executor'] == 'dots'
    assert hive_cli(monkeypatch, cfg, 'show', cid) == 0
    assert 'executor: dots' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'list') == 0
    assert 'executor=dots' in capsys.readouterr().out
    assert hive_cli(monkeypatch, cfg, 'show', cid, '--json') == 0
    assert json.loads(capsys.readouterr().out)['executor'] == 'dots'
    unconfigured = dict(cfg)
    del unconfigured['executors']
    before = conn.execute('SELECT count(*) FROM cards').fetchone()[0]
    for kw in ({'executor': 'nope', 'cfg': cfg}, {'executor': 'dots', 'cfg': unconfigured},
               {'executor': 'dots', 'cfg': cfg, 'assignee': 'claude', 'executors': EXECUTORS},
               {'executor': 'dots', 'cfg': cfg, 'model': 'gpt-6-astra'},
               {'executor': 'dots', 'cfg': cfg, 'panel': True},
               {'executor': 'dots', 'cfg': cfg, 'kind': 'epic'},
               {'executor': 'dots', 'cfg': cfg, 'kind': 'recurring', 'every': 60, 'max_runs': 2}):
        with pytest.raises(api.CardError):
            mk(conn, 'refused', **kw)
    for flags in (('--executor', 'nope'), ('--executor', 'dots', '--assignee', 'codex'),
                  ('--executor', 'dots', '--model', 'gpt-6-astra'), ('--executor', 'dots', '--panel'),
                  ('--executor', 'dots', '--epic'), ('--assignee', 'dots')):
        assert hive_cli(monkeypatch, cfg, 'create', '--title', 'refused', '--goal', 'g', '--acceptance', 'true',
                        '--step', '1', '--source', 'system:test', '--created-by', 'secretary', *flags) == 2
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == before


def test_dots_run_takes_no_slot_survives_reap_and_polls_at_interval(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    heads_cards = [mk(conn, '[ok] head a'), mk(conn, '[ok] head b')]
    launched = d.tick()['launched']
    runs = {r['card_id']: r for r in conn.execute('SELECT * FROM runs').fetchall()}
    assert len(launched) == 3 and set(runs) == {cid, *heads_cards}
    run = runs[cid]
    assert (run['status'], run['job'], run['vendor']) == ('running', 'job-1', 'dots')
    sent = dots_calls(cfg)
    assert [c['cmd'] for c in sent] == ['send']
    assert f'# Card {cid}: dots card' in sent[0]['args'][0] and cfg['texts']['report_request'] in sent[0]['args'][0]
    clock.offset = 30
    d.tick()
    assert conn.execute('SELECT status FROM runs WHERE id=?', (run['id'],)).fetchone()['status'] == 'running'
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send']
    clock.offset = 61
    d.tick()
    d.tick()
    assert dots_calls(cfg)[1:] == [{'cmd': 'get', 'args': ['job-1', '--json']}]
    assert conn.execute('SELECT status FROM runs WHERE id=?', (run['id'],)).fetchone()['status'] == 'running'
    assert api.get(conn, cid)['state'] == 'running'


def test_dots_restart_resumes_saved_job_and_gates_the_report(env):
    cfg, conn, _ = env
    clock = Clock()
    cid = dots_card(conn, cfg)
    dots_dispatcher(cfg, conn, clock).tick()
    out = dots_state(cfg) / 'out.txt'
    out.write_text('42\n')
    dots_reply(cfg, 'Сделано: out.txt', [out])
    clock.offset = 61
    restarted = dots_dispatcher(cfg, store.connect(cfg['db']), clock)
    restarted.tick()
    card = api.get(conn, cid)
    run = card['runs'][0]
    workdir = Path(card['workdir'])
    assert card['state'] == 'done' and len(card['runs']) == 1
    assert (run['status'], run['exit_code'], run['report']) == ('exited', 0, 'Сделано: out.txt')
    assert (workdir / 'REPORT.md').read_text() == 'Сделано: out.txt' and (workdir / 'out.txt').read_text() == '42\n'
    assert card['gates'][0]['passed'] == 1
    assert [(c['cmd'], c['args'][0]) for c in dots_calls(cfg)] == [('send', run['prompt']), ('get', 'job-1')]


def test_dots_send_crash_blocks_with_unknown_outcome_and_never_resends(env):
    cfg, conn, _ = env
    cid = dots_card(conn, cfg)
    dots_state(cfg).mkdir()
    (dots_state(cfg) / 'send-mode').write_text('die')
    p = subprocess.run([sys.executable, str(ROOT / 'scripts' / 'hive.py'), '--config', cfg['_path'], 'tick'],
                       capture_output=True, text=True, env=dict(os.environ, HIVE_DB=cfg['db']), timeout=60)
    assert p.returncode == -signal.SIGKILL
    run = conn.execute('SELECT * FROM runs WHERE card_id=?', (cid,)).fetchone()
    assert (run['status'], run['job']) == ('starting', None)
    (dots_state(cfg) / 'send-mode').unlink()
    d = dots_dispatcher(cfg, store.connect(cfg['db']), Clock())
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and 'send outcome unknown' in card['block_reason']
    assert len(card['runs']) == 1 and 'retry' not in kinds(conn, cid)
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send']


def test_dots_gate_failure_starts_a_new_round_then_blocks(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    d.tick()
    dots_reply(cfg, 'no file produced')
    clock.offset = 61
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['round']) == ('running', 2) and 'retry' in kinds(conn, cid)
    clock.offset = 122
    d.tick()
    card = api.get(conn, cid)
    sends = [c['args'][0] for c in dots_calls(cfg) if c['cmd'] == 'send']
    assert card['state'] == 'blocked' and len(sends) == 2 and len(card['gates']) == 1
    assert 'loop_stopped' in kinds(conn, cid)
    assert 'Previous round failed' in sends[1] and 'Previous round failed' not in sends[0]



@pytest.mark.parametrize('same', [False, True])
def test_dots_long_report_full_identity_and_tail(env, same):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    prefix = 'RESULT: partial — owner: проверить конец.\n' + 'x' * 61000
    first = prefix + '\nTAIL BLOCKER 1'
    second = prefix + ('\nTAIL BLOCKER 1' if same else '\nTAIL BLOCKER 2')
    d.tick()
    dots_reply(cfg, first)
    clock.offset = 61
    d.tick()
    dots_reply(cfg, second)
    clock.offset = 122
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked'
    assert ('identical worker reports' in card['block_reason']) is same
    payload = d.result_payload(cid, 'blocked', 'test')
    assert payload['result_line'] == first.splitlines()[0]
    assert second[-100:] in payload['report_excerpt']
    assert len(card['gates']) == (1 if same else 2)


def test_dots_review_uses_a_ladder_reviewer(env):
    cfg, conn, d = env
    cfg['executors']['dots']['poll_s'] = 0
    run_heads_inline(cfg, d)
    cid = dots_card(conn, cfg, title='dots reviewed', review=True)
    d.tick()
    out = dots_state(cfg) / 'out.txt'
    out.write_text('42\n')
    dots_reply(cfg, 'done', [out])
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'done' and 'review_requested' in kinds(conn, cid)
    worker, reviewer = card['runs']
    assert (worker['vendor'], worker['status']) == ('dots', 'exited')
    assert (reviewer['role'], reviewer['status'], reviewer['verdict']) == ('reviewer', 'exited', 'PASS')
    assert reviewer['vendor'] in heads.VENDORS


def test_dots_review_fail_hands_its_essentials_not_its_notes_to_the_next_round(env):
    cfg, conn, d = env
    cfg['executors']['dots']['poll_s'] = 0
    run_heads_inline(cfg, d)
    cid = dots_card(conn, cfg, title='[reviewfail][reviewnotes] dots reviewed', review=True)
    d.tick()
    out = dots_state(cfg) / 'out.txt'
    out.write_text('42\n')
    dots_reply(cfg, 'done', [out])
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    [reviewer] = [r for r in card['runs'] if r['role'] == 'reviewer']
    assert reviewer['verdict'] == 'FAIL' and 'minor wording' in reviewer['report']
    assert card['feedback'] == 'VERDICT: FAIL\nmissing thing'


def dots_get_mode(cfg, mode):
    dots_state(cfg).mkdir(exist_ok=True)
    path = dots_state(cfg) / 'get-mode'
    if mode is None:
        path.unlink()
    else:
        path.write_text(mode)


def write_dots_config(tmp_path, cfg, section):
    text = Path(cfg['_path']).read_text()
    start = text.index('[executors.dots]')
    end = text.index('[budget]')
    path = tmp_path / 'executor.toml'
    path.write_text(text[:start] + section + '\n' + text[end:])
    return path


def test_dots_config_refuses_a_missing_or_non_positive_limit(env, tmp_path):
    cfg, _, _ = env
    full = {'command': '["dots"]', 'poll_s': '60', 'call_timeout_s': '30', 'timeout_s': '600', 'cooldown_s': '300',
            'max_parallel': '2', 'daily_sends': '20'}
    loaded = store.load_config(write_dots_config(
        tmp_path, cfg, '[executors.dots]\n' + ''.join(f'{k} = {v}\n' for k, v in full.items())))
    assert loaded['executors']['dots']['timeout_s'] == 600
    for key in full:
        for value in (None, '0', '-5') if key != 'command' else (None, '[]'):
            section = {**full, key: value}
            body = ''.join(f'{k} = {v}\n' for k, v in section.items() if v is not None)
            with pytest.raises(ValueError, match=f'executor dots: {key}'):
                store.load_config(write_dots_config(tmp_path, cfg, '[executors.dots]\n' + body))
    assert store.load_config(ROOT / 'config.example.toml')['executors']['dots']['timeout_s'] == 3600


def test_dots_timeout_fails_the_round_resends_then_blocks(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    d.tick()
    clock.offset = 590
    d.tick()
    first = api.get(conn, cid)['runs'][0]
    assert first['status'] == 'running' and [c['cmd'] for c in dots_calls(cfg)] == ['send', 'get']
    clock.offset = 610
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    first, second = card['runs']
    assert (first['status'], card['state'], card['round']) == ('timeout', 'running', 2)
    assert 'job-1' in first['error'] and '600s' in first['error']
    assert [(e['data']['round']) for e in events_of(conn, cid, 'retry')] == [1]
    assert (second['status'], second['job']) == ('running', 'job-2')
    clock.offset = 1300
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'blocked' and card['runs'][1]['status'] == 'timeout'
    assert 'job-2' in card['block_reason']
    assert [(c['cmd'], c['args'][0]) for c in dots_calls(cfg)][1:] == [('get', 'job-1'), ('send', second['prompt'])]
    assert len(card['runs']) == 2


@pytest.mark.parametrize('mode,cls,text', [('fail', 'error', 'boom'), ('auth', 'auth', 'invalid_auth'),
                                           ('garbage', 'error', 'answer unusable')])
def test_dots_failed_get_keeps_polling_and_finishes_after_an_answer(env, mode, cls, text):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    d.tick()
    run_id = api.get(conn, cid)['runs'][0]['id']
    dots_get_mode(cfg, mode)
    dots_reply(cfg, 'Сделано: out.txt')
    clock.offset = 61
    d.tick()
    clock.offset = 90
    d.tick()
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send', 'get']
    clock.offset = 122
    d.tick()
    run = conn.execute('SELECT * FROM runs WHERE id=?', (run_id,)).fetchone()
    assert (run['status'], run['job']) == ('running', 'job-1')
    failed = events_of(conn, cid, 'external_poll_failed')
    assert len(failed) == 1 and failed[0]['data']['class'] == cls and text in failed[0]['data']['error']
    anomaly = [a for a in dispatcher.observe(conn, cfg, now=clock()) if a['kind'] == 'external_poll_failed']
    assert anomaly == [{'kind': 'external_poll_failed', 'card': cid, 'executor': 'dots', 'class': cls,
                        'error': failed[0]['data']['error']}]
    dots_get_mode(cfg, None)
    out = Path(api.get(conn, cid)['workdir']) / 'out.txt'
    out.write_text('42\n')
    clock.offset = 183
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'done' and len(card['runs']) == 1
    assert (Path(card['workdir']) / 'REPORT.md').read_text() == 'Сделано: out.txt'
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send', 'get', 'get', 'get']
    assert not [a for a in dispatcher.observe(conn, cfg, now=clock()) if a['kind'] == 'external_poll_failed']


def test_dots_get_that_cannot_run_or_times_out_keeps_the_run(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    d.tick()
    command = cfg['executors']['dots']['command']
    cfg['executors']['dots']['command'] = [str(dots_state(cfg) / 'missing-client')]
    clock.offset = 61
    d.tick()
    cfg['executors']['dots']['command'] = command
    # The client hangs until the call timeout kills it, so TimeoutExpired never depends on how fast the host runs
    # a real process against a 1ms budget (a stalled test process let the client finish first: no timeout).
    dots_get_mode(cfg, 'hang')
    cfg['executors']['dots']['call_timeout_s'] = 0.5
    clock.offset = 122
    d.tick()
    run = api.get(conn, cid)['runs'][0]
    errors = [e['data']['error'] for e in events_of(conn, cid, 'external_poll_failed')]
    assert (run['status'], run['job']) == ('running', 'job-1') and len(errors) == 2
    assert 'FileNotFoundError' in errors[0] and 'TimeoutExpired' in errors[1]


def test_observe_counts_a_dots_card_stuck_from_its_executor_timeout(env):
    cfg, conn, _ = env
    clock = Clock()
    cid = dots_card(conn, cfg)
    dots_dispatcher(cfg, conn, clock).tick()
    head = mk(conn, '[ok] head')
    conn.execute("UPDATE cards SET state='running', updated_at=? WHERE id=?", (time.time(), head))
    stuck = {a['card'] for a in dispatcher.observe(conn, cfg, now=time.time() + 901) if a['kind'] == 'stuck'}
    assert stuck == {cid}


def test_dots_config_refuses_fractional_counts_and_a_bad_refused_type_list(env, tmp_path):
    cfg, _, _ = env
    full = {'command': '["dots"]', 'poll_s': '60', 'call_timeout_s': '30', 'timeout_s': '600', 'cooldown_s': '0.5',
            'max_parallel': '2', 'daily_sends': '20'}
    loaded = store.load_config(write_dots_config(
        tmp_path, cfg, '[executors.dots]\n' + ''.join(f'{k} = {v}\n' for k, v in full.items())))
    assert loaded['executors']['dots']['cooldown_s'] == 0.5
    for key, value in (('max_parallel', '1.5'), ('daily_sends', 'true'), ('refuse_task_types', '"code"'),
                       ('refuse_task_types', '["code", 1]')):
        body = ''.join(f'{k} = {v}\n' for k, v in {**full, key: value}.items())
        with pytest.raises(ValueError, match=f'executor dots: {key}'):
            store.load_config(write_dots_config(tmp_path, cfg, '[executors.dots]\n' + body))


def test_dots_refuses_a_coding_task_type_before_write(env, monkeypatch, capsys):
    cfg, conn, _ = env
    with pytest.raises(api.CardError, match="task type 'Код'.*dots-delegation skill"):
        dots_card(conn, cfg, task_type='Код')
    assert hive_cli(monkeypatch, cfg, 'create', '--title', 'fix bug', '--goal', 'g', '--acceptance', 'true',
                    '--step', '1', '--source', 'system:test', '--created-by', 'secretary', '--executor', 'dots',
                    '--type', 'code') == 2
    assert "task type 'code'" in capsys.readouterr().err
    assert conn.execute('SELECT count(*) FROM cards').fetchone()[0] == 0
    assert api.get(conn, dots_card(conn, cfg, task_type='research'))['executor'] == 'dots'


def test_dots_third_card_waits_for_a_parallel_slot_and_leaves_when_one_finishes(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    first = dots_card(conn, cfg, title='first')
    d.tick()
    clock.offset = 30
    second = dots_card(conn, cfg, title='second')
    d.tick()
    third = dots_card(conn, cfg, title='third')
    d.tick()
    d.tick()
    wait = {'executor': 'dots', 'reason': 'dots max_parallel 2 reached'}
    card = api.get(conn, third)
    assert (card['state'], card['round'], card['runs'], card['executor_wait']) == ('queued', 0, [], wait)
    assert [e['data'] for e in events_of(conn, third, 'executor_wait')] == [wait]
    assert {'kind': 'executor_wait', 'card': third, **wait} in dispatcher.observe(conn, cfg, now=clock())
    out = dots_state(cfg) / 'out.txt'
    out.write_text('42\n')
    dots_reply(cfg, 'done', [out])
    clock.offset = 61
    d.tick()
    states = {cid: api.get(conn, cid)['state'] for cid in (first, second, third)}
    assert states == {first: 'done', second: 'running', third: 'running'}
    card = api.get(conn, third)
    assert card['executor_wait'] is None and card['runs'][0]['job'] == 'job-3'
    assert not [a for a in dispatcher.observe(conn, cfg, now=clock()) if a['kind'] == 'executor_wait']


def test_dots_daily_sends_hold_a_card_until_the_next_day(env):
    cfg, conn, _ = env
    cfg['executors']['dots']['daily_sends'] = 1
    clock = Clock()
    clock.offset = heads.day_start(cfg['owner_tz'], time.time()) + 43200 - time.time()
    d = dots_dispatcher(cfg, conn, clock)
    first = dots_card(conn, cfg, title='first')
    d.tick()
    out = dots_state(cfg) / 'out.txt'
    out.write_text('42\n')
    dots_reply(cfg, 'done', [out])
    second = dots_card(conn, cfg, title='second')
    clock.offset += 61
    d.tick()
    assert api.get(conn, first)['state'] == 'done'
    card = api.get(conn, second)
    assert card['state'] == 'queued' and card['executor_wait']['reason'] == 'dots daily_sends 1 reached'
    clock.offset += 3600
    d.tick()
    assert api.get(conn, second)['state'] == 'queued'
    clock.offset += 86400
    d.tick()
    card = api.get(conn, second)
    assert (card['state'], card['runs'][0]['job']) == ('running', 'job-2')


def test_dots_send_auth_opens_a_cooldown_without_spending_a_round(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    cid = dots_card(conn, cfg)
    dots_state(cfg).mkdir()
    (dots_state(cfg) / 'send-mode').write_text('auth')
    d.tick()
    card = api.get(conn, cid)
    run = card['runs'][0]
    assert (card['state'], card['round']) == ('queued', 0) and 'retry' not in kinds(conn, cid)
    assert (run['status'], run['exit_code'], run['outcome']) == ('failed_launch', 3, 'auth')
    cooldown = events_of(conn, cid, 'executor_cooldown')
    assert [(e['data']['executor'], e['data']['reason']) for e in cooldown] == [('dots', 'auth')]
    clock.offset = 299
    d.tick()
    card = api.get(conn, cid)
    assert card['state'] == 'queued' and card['executor_wait']['reason'].startswith('dots cooldown until')
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send'] and len(events_of(conn, cid, 'executor_cooldown')) == 1
    (dots_state(cfg) / 'send-mode').unlink()
    clock.offset = 301
    d.tick()
    card = api.get(conn, cid)
    assert (card['state'], card['round'], card['executor_wait']) == ('running', 1, None)
    assert card['runs'][1]['job'] == 'job-2' and card['runs'][1]['round'] == 1
    assert 'retry' not in kinds(conn, cid)


def test_dots_get_auth_opens_a_cooldown_once_per_episode(env):
    cfg, conn, _ = env
    clock = Clock()
    d = dots_dispatcher(cfg, conn, clock)
    polled = dots_card(conn, cfg, title='polled')
    d.tick()
    dots_get_mode(cfg, 'auth')
    clock.offset = 61
    d.tick()
    later = dots_card(conn, cfg, title='later')
    clock.offset = 122
    d.tick()
    assert api.get(conn, polled)['runs'][0]['status'] == 'running'
    assert len(events_of(conn, polled, 'executor_cooldown')) == 1
    card = api.get(conn, later)
    assert card['state'] == 'queued' and card['executor_wait']['reason'].startswith('dots cooldown until')
    clock.offset = 362
    d.tick()
    assert len(events_of(conn, polled, 'executor_cooldown')) == 2
    assert api.get(conn, later)['state'] == 'queued'
    dots_get_mode(cfg, None)
    clock.offset = 663
    d.tick()
    assert api.get(conn, later)['state'] == 'running'


def test_dots_fallback_runs_a_head_when_admission_refuses(env, monkeypatch, capsys):
    cfg, conn, d = env
    cfg['executors']['dots']['max_parallel'] = 1
    run_heads_inline(cfg, d)
    busy = dots_card(conn, cfg, title='busy')
    assert hive_cli(monkeypatch, cfg, 'create', '--title', '[ok] fallback', '--goal', 'g',
                    '--acceptance', 'test -s out.txt', '--step', '1', '--source', 'system:test', '--created-by', 'secretary',
                    '--executor', 'dots', '--on-unavailable', 'fallback') == 0
    cid = capsys.readouterr().out.split()[0]
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    worker = workers(card)[0]
    assert card['state'] == 'done' and card['executor'] == 'dots' and worker['vendor'] in heads.VENDORS
    assert [e['data'] for e in events_of(conn, cid, 'executor_fallback')] == [
        {'run': worker['id'], 'round': 1, 'executor': 'dots', 'vendor': worker['vendor'], 'model': worker['model'],
         'reason': 'dots max_parallel 1 reached'}]
    assert worker['model'] == cfg['ladder'][1][worker['vendor']]['model']
    assert '# Card' in worker['prompt'] and cfg['texts']['report_request'] not in worker['prompt']
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send'] and api.get(conn, busy)['state'] == 'running'


def test_dots_fallback_runs_a_head_for_the_round_after_a_timeout(env):
    cfg, conn, d = env
    clock = Clock()
    d.clock = clock
    run_heads_inline(cfg, d)
    cid = dots_card(conn, cfg, title='[ok] slow dots', on_unavailable='fallback')
    d.tick()
    clock.offset = 610
    d.tick()
    d.tick()
    card = api.get(conn, cid)
    dots_run, head_run = workers(card)
    assert (dots_run['vendor'], dots_run['status'], head_run['vendor'] in heads.VENDORS) == ('dots', 'timeout', True)
    assert card['state'] == 'done' and head_run['round'] == 2 and card['executor'] == 'dots'
    fallback = events_of(conn, cid, 'executor_fallback')
    assert len(fallback) == 1 and fallback[0]['data']['reason'] == dots_run['error']
    assert [c['cmd'] for c in dots_calls(cfg)] == ['send']


@pytest.mark.parametrize('fallback', [None, False, True])
def test_review_same_vendor_requires_explicit_opt_in(env, fallback):
    cfg, conn, d = env
    cfg['vendors']['claude']['disabled'] = True
    if fallback is not None:
        cfg['review_same_vendor_fallback'] = fallback
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] single-vendor review', review=True)
    for _ in range(5):
        d.tick()
    card = api.get(conn, cid)
    reviewers = [r for r in card['runs'] if r['role'] == 'reviewer']
    if fallback:
        assert card['state'] == 'done'
        assert len(reviewers) == 1 and reviewers[0]['vendor'] == 'codex'
        assert reviewers[0]['verdict'] == 'PASS'
    else:
        assert card['state'] == 'review' and reviewers == []


def test_review_fallback_prefers_other_vendor(env):
    cfg, conn, d = env
    cfg['review_same_vendor_fallback'] = True
    run_heads_inline(cfg, d)
    cid = mk(conn, '[ok] prefer independent review', review=True)
    assert tick_until_terminal(d, conn, cid) == 'done'
    worker, reviewer = api.get(conn, cid)['runs']
    assert worker['vendor'] != reviewer['vendor']
