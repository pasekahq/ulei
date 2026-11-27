"""The stall watchdog (ADR-0101) on a fake hive store, fake qb limits, a fake journal and escalation source, a fake
`ulei-tg` and an injected clock, with no dispatcher ticking: each stall condition makes exactly one `watchdog_alert`, a
healthy hive none, a repeat only after its cooldown; an alert nobody claims or whose claim falls silent is sent once as
it is with its receipt recorded and the secretary told, a withdrawn one never, and a failed send is retried and not
delivered."""
import fcntl
import json
import sys
from pathlib import Path

import pytest
from fake_delivery import FakeAdapter
from test_delivery_adapter import c1, ulei_keys  # noqa: F401
from test_hive import env, mk, mk_series  # noqa: F401
from test_reminder import cli, receipt, tg_answers, tg_calls
from hive import api, delivery, store, watchdog

START = 100000.0
FAKE_SOURCE = Path(__file__).resolve().parent / 'fake_source.py'


class Hive:
    """The test hive at `now`: `run` makes one watchdog pass with the [watchdog] table `section` and the qb limits
    `limits`; the events it publishes reach `adapter`."""

    def __init__(self, cfg, conn, now, adapter, tmp):
        self.cfg, self.conn, self.now, self.adapter, self.tmp = cfg, conn, now, adapter, tmp
        self.section, self.limits = {}, {}

    def run(self, at, conn=None, cfg=None):
        self.now[0] = at
        cfg = dict(cfg or self.cfg, watchdog=watchdog.config({'watchdog': self.section}))
        return watchdog.Watchdog(cfg, conn or self.conn, limits_fetcher=lambda c: dict(self.limits),
                                 clock=lambda: self.now[0], sleep=lambda s: None).run()

    def events(self, kind):
        return [(e.id, json.loads(e.payload)) for e in self.adapter.events if e.kind == kind]

    def source(self, name, stdout='', code=0):
        """The argv of a fake source printing `stdout` and exiting `code`; its calls are in `<name>/calls.jsonl`."""
        root = self.tmp / name
        root.mkdir(exist_ok=True)
        (root / 'stdout').write_text(stdout)
        if code:
            (root / 'exit').write_text(str(code))
        return [sys.executable, str(FAKE_SOURCE), str(root)]

    def calls(self, name):
        return [json.loads(line) for line in (self.tmp / name / 'calls.jsonl').read_text().splitlines()]


@pytest.fixture
def hive(env, monkeypatch, tmp_path):
    cfg, conn, _ = env
    adapter = FakeAdapter()
    monkeypatch.setitem(delivery.ADAPTERS, 'fake', lambda c: adapter)
    now = [START]
    monkeypatch.setattr(api.time, 'time', lambda: now[0])
    monkeypatch.setenv('HIVE_DB', cfg['db'])
    return Hive(dict(cfg, delivery='fake'), conn, now, adapter, tmp_path)


def running(conn, title):
    """A card a head works on, as the dispatcher leaves it."""
    cid = mk(conn, title)
    conn.execute("UPDATE cards SET state='running', round=1, version=version+1 WHERE id=?", (cid,))
    return cid


def nudge(conn, cid, *pids):
    for pid in pids:
        conn.execute("INSERT INTO events(card_id,at,kind,actor,data) VALUES(?,?,'wait_nudge','dispatcher',?)",
                     (cid, START, json.dumps({'run': 1, 'round': 1, 'pid': pid, 'command': 'sleep 3600',
                                              'waited_s': 1500.0, 'count': 1, 'live': False, 'text': 'synthetic'})))


def cooldown(conn, vendor='claude', until=START + 86400):
    with store.tx(conn):
        store.meta_set(conn, f'provider_cooldown:{vendor}', {'failures': 3, 'until': until})


def series_runs(conn, enders):
    """A series whose ended instances, period by period, were each cancelled by an actor of `enders`."""
    sid = mk_series(conn, every=3600, max_runs=10)
    runs = []
    for period, actor in enumerate(enders):
        cid = mk(conn, f'synthetic run {period}')
        conn.execute("""UPDATE cards SET state='cancelled', wait_reason=NULL, series_period=?, result=?,
                        version=version+1 WHERE id=?""", (period, actor, cid))
        conn.execute("INSERT INTO links(card_id,kind,target_id,created_at) VALUES(?,'series',?,?)", (cid, sid, START))
        conn.execute("INSERT INTO events(card_id,at,kind,actor,data) VALUES(?,?,'cancelled',?,?)",
                     (cid, START, actor, json.dumps({'reason': actor})))
        runs.append(cid)
    return sid, runs


def journal_line(at, message):
    return json.dumps({'__REALTIME_TIMESTAMP': str(int(at * 1e6)), 'MESSAGE': message}) + '\n'


def stalled_queue(h):
    mk(h.conn, 'synthetic waiting card')
    return (START + 900, 'queue_stalled',
            {'cards': 1, 'oldest_wait_s': 900, 'active_runs': 0, 'last_run_start_s': None, 'tick_age_s': None},
            'Hive watchdog: 1 card(s) wait to run, the oldest for 15 min, and no head has ever started; the dispatcher '
            'has never ticked.')


def vendor_cooling_down(h):
    cooldown(h.conn, until=START + 1800)
    return (START, 'vendor_cooldown:claude', {'vendor': 'claude', 'failures': 3, 'until': START + 1800},
            'Hive watchdog: vendor claude is cooling down after 3 provider failures; no head of it starts for another '
            '30 min.')


def qb_near_its_limit(h):
    h.limits = {'claude': 8.0, 'codex': 60.0}
    return (START, 'qb_limit:claude',
            {'provider': 'claude', 'window': 'week', 'left_pct': 8.0, 'usage_pct': 92.0, 'exhausted': False},
            'Hive watchdog: claude has used 92% of its weekly limit in qb.')


def three_superseded(h):
    sid, runs = series_runs(h.conn, ['superseded'] * 3)
    return (START, f'series_superseded:{sid}', {'series': sid, 'instances': runs, 'periods': [0, 1, 2]},
            f'Hive watchdog: the last 3 runs of series {sid} were each superseded by the next before they ran.')


def unread_escalation(h):
    h.section['escalations_command'] = h.source('escalations', json.dumps([
        {'id': 'esc-1', 'created_at': START - 1800, 'read': False},
        {'id': 'esc-2', 'created_at': START - 1000, 'read': False},
        {'id': 'esc-3', 'created_at': START - 7200, 'read': True}]))
    return (START, 'escalation_unread:esc-1', {'escalation': 'esc-1', 'created_at': START - 1800, 'age_s': 1800},
            'Hive watchdog: escalation esc-1 has been unread for 30 min.')


def refused_token_refresh(h):
    h.section['bridge_unit'] = 'synthetic-bridge.service'
    h.section['journal_command'] = h.source('journal', journal_line(START - 1200, 'token refresh refused: old')
                                            + journal_line(START - 60, 'mcp: token refresh refused by the provider')
                                            + journal_line(START - 30, 'mcp: serving'))
    return (START, 'bridge_token_refused', {'lines': 1, 'last_at': START - 60, 'window_s': 600},
            'Hive watchdog: the MCP bridge refused a token refresh 1 time(s) in the last 10 min.')


def nudged_three_times(h):
    cid = running(h.conn, 'synthetic nudged card')
    nudge(h.conn, cid, 41, 41, 41)
    return (START, f'wait_nudge_repeat:{cid}:41', {'card': cid, 'pid': 41, 'nudges': 3, 'waited_s': 1500.0},
            f'Hive watchdog: card {cid} got 3 wait nudges in a row on one command (pid 41), which has run for 25 min.')


@pytest.mark.parametrize('arrange', [stalled_queue, vendor_cooling_down, qb_near_its_limit, three_superseded,
                                     unread_escalation, refused_token_refresh, nudged_three_times])
def test_each_condition_fires_exactly_one_watchdog_alert(hive, tmp_path, arrange):
    at, key, facts, text = arrange(hive)
    condition = key.split(':')[0]
    if at > START:
        assert hive.run(at - 1)['fired'] == []
    summary = hive.run(at)
    [fired] = summary['fired']
    cid = fired['card']
    assert fired == {'condition': condition, 'key': key, 'card': cid} and summary['errors'] == []
    assert summary['published'] == 1
    [(event_id, payload)] = hive.events('watchdog_alert')
    assert event_id == f'hive:{cid}:watchdog_alert' and [e.kind for e in hive.adapter.events] == ['watchdog_alert']
    assert (payload['condition'], payload['key'], payload['facts'], payload['text']) == (condition, key, facts, text)
    assert (payload['card_id'], payload['due_at'], payload['unclaimed_s'], payload['claim_silence_s']) == (
        cid, at, 180, 300)
    assert (payload['source'], payload['next_actor'], payload['note']) == ('hive', 'secretary', watchdog.ALERT_NOTE)
    card = api.get(hive.conn, cid)
    assert (card['state'], card['wait_reason'], card['reminder'], card['created_by']) == (
        'blocked', 'reminder', text, 'watchdog')
    assert card['reminder_state']['state'] == 'due' and card['runs'] == [] and tg_calls(tmp_path) == []
    assert hive.run(at + 60)['fired'] == [] and len(hive.events('watchdog_alert')) == 1
    if condition == 'bridge_token_refused':
        assert hive.calls('journal')[0] == ['--unit=synthetic-bridge.service', f'--since=@{int(at - 600)}',
                                            '--output=json', '--no-pager', '--quiet']


def test_a_healthy_hive_fires_none(hive, tmp_path):
    conn = hive.conn
    mk(conn, 'synthetic waiting card')
    busy = running(conn, 'synthetic busy card')
    conn.execute("""INSERT INTO runs(card_id,round,role,vendor,model,effort,step,prompt,status,started_at)
                    VALUES(?,1,'worker','claude','claude-small','medium',1,'p','running',?)""", (busy, START + 3000))
    nudge(conn, busy, 41, 41, 42, 41, 41)
    cooldown(conn, until=START + 3600 - 1)
    hive.limits = {'claude': 11.0}
    series_runs(conn, ['superseded', 'owner', 'superseded'])
    hive.section.update(
        escalations_command=hive.source('escalations', json.dumps([
            {'id': 'esc-1', 'created_at': START + 3600 - 1799, 'read': False},
            {'id': 'esc-2', 'created_at': START, 'read': True}])),
        bridge_unit='synthetic-bridge.service',
        journal_command=hive.source('journal', journal_line(START + 3600 - 601, 'token refresh refused')
                                    + journal_line(START + 3500, 'mcp: token refresh done')))
    hive.now[0] = START + 3600 - 10
    with store.tx(conn):
        store.meta_set(conn, 'dispatcher_tick', {'advanced': 0})
    summary = hive.run(START + 3600)
    assert summary == {'sent': 0, 'lock': 'held', 'fired': [], 'published': 0, 'errors': []}
    assert hive.adapter.events == [] and tg_calls(tmp_path) == []


def test_a_condition_repeats_only_after_its_cooldown(hive):
    cooldown(hive.conn)
    [first] = hive.run(START)['fired']
    assert hive.run(START + 3599)['fired'] == []
    [second] = hive.run(START + 3600)['fired']
    assert first['key'] == second['key'] == 'vendor_cooldown:claude' and first['card'] != second['card']
    hive.section['cooldown_s'] = {'vendor_cooldown': 60}
    assert hive.run(START + 3659)['fired'] == []
    assert len(hive.run(START + 3660)['fired']) == 1
    assert len(hive.events('watchdog_alert')) == 3


def test_wait_nudge_rule_alerts_once_per_card_and_pid_on_three_in_a_row(hive):
    conn = hive.conn
    two, mixed, both = (running(conn, f'synthetic card {n}') for n in range(3))
    nudge(conn, two, 41, 41)
    nudge(conn, mixed, 41, 42, 41, 42)
    nudge(conn, both, 41, 41, 41, 42, 42, 42)
    assert {f['key'] for f in hive.run(START)['fired']} == {f'wait_nudge_repeat:{both}:41',
                                                             f'wait_nudge_repeat:{both}:42'}
    nudge(conn, two, 41)
    nudge(conn, both, 42)
    assert [f['key'] for f in hive.run(START + 60)['fired']] == [f'wait_nudge_repeat:{two}:41']
    assert hive.run(START + 30 * 86400)['fired'] == []
    assert len(hive.events('watchdog_alert')) == 3


def alert(hive):
    """One alert made at START (the vendor stays cooling down for a day); its card and text."""
    cooldown(hive.conn)
    [fired] = hive.run(START)['fired']
    return fired['card'], api.get(hive.conn, fired['card'])['reminder']


def test_unclaimed_alert_is_sent_once_as_it_is_and_the_secretary_told(hive, tmp_path):
    cid, text = alert(hive)
    assert hive.run(START + 179)['sent'] == 0 and tg_calls(tmp_path) == []
    summary = hive.run(START + 180)
    assert (summary['sent'], summary['lock'], summary['published']) == (1, 'held', 1)
    assert tg_calls(tmp_path) == [{'argv': ['notify', '--config', hive.cfg['reminders']['tg_config']], 'stdin': text}]
    card = api.get(hive.conn, cid)
    sent = card['reminder_state']
    assert (card['state'], card['result']) == ('done', text)
    assert (sent['state'], sent['sent_by'], sent['sent_text'], sent['receipt']) == ('delivered', 'watchdog', text,
                                                                                    receipt(101))
    [(event_id, payload)] = hive.events('reminder_fallback')
    assert event_id == f'hive:{cid}:reminder_fallback'
    assert (payload['text'], payload['why'], payload['claimed_at'], payload['receipt']) == (
        text, 'unclaimed', None, receipt(101))
    hive.run(START + 1800)
    assert len(tg_calls(tmp_path)) == 1 and len(hive.events('reminder_fallback')) == 1


def test_claimed_alert_is_sent_once_when_its_claim_falls_silent(hive, tmp_path, capsys):
    cid, text = alert(hive)
    hive.now[0] = START + 100
    assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    hive.run(START + 180)
    hive.run(START + 399)
    assert tg_calls(tmp_path) == [] and api.get(hive.conn, cid)['reminder_state']['state'] == 'claimed'
    assert hive.run(START + 400)['sent'] == 1
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [text]
    [(_, payload)] = hive.events('reminder_fallback')
    assert (payload['why'], payload['claimed_at'], payload['receipt']) == ('claim_silent', START + 100, receipt(101))
    assert api.get(hive.conn, cid)['reminder_state']['sent_by'] == 'watchdog'


def test_withdrawn_alert_is_never_sent(hive, tmp_path, capsys):
    cid, _ = alert(hive)
    hive.now[0] = START + 60
    assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    assert cli(capsys, 'reminder', 'withdraw', cid, '--reason', 'the owner already knows')[0] == 0
    assert hive.run(START + 3000)['sent'] == 0
    card = api.get(hive.conn, cid)
    assert (card['state'], card['reminder_state']['state'], card['reminder_state']['receipt']) == (
        'cancelled', 'withdrawn', None)
    assert tg_calls(tmp_path) == [] and hive.events('reminder_fallback') == []


def test_secretary_sends_an_edited_alert_and_the_watchdog_sends_nothing(hive, tmp_path, capsys):
    cid, _ = alert(hive)
    hive.now[0] = START + 60
    assert cli(capsys, 'reminder', 'claim', cid)[0] == 0
    code, out, _ = cli(capsys, 'reminder', 'send', cid, '--text', 'The hive pauses claude for a while.')
    assert code == 0 and json.loads(out)['receipt'] == receipt(101)
    assert hive.run(START + 3000)['sent'] == 0
    assert [c['stdin'] for c in tg_calls(tmp_path)] == ['The hive pauses claude for a while.']
    assert api.get(hive.conn, cid)['reminder_state']['sent_by'] == 'secretary' and hive.events('reminder_fallback') == []


@pytest.mark.parametrize('answer', [1, 'no-receipt'])
def test_failed_send_is_retried_and_never_marked_delivered(hive, tmp_path, answer):
    cid, text = alert(hive)
    tg_answers(tmp_path, answer)
    assert hive.run(START + 180)['sent'] == 1
    failed = api.get(hive.conn, cid)['reminder_state']
    assert (failed['state'], failed['attempts'], failed['receipt'], failed['retry_at']) == ('due', 1, None, START + 240)
    assert api.get(hive.conn, cid)['state'] == 'blocked' and hive.events('reminder_fallback') == []
    assert hive.run(START + 239)['sent'] == 0 and len(tg_calls(tmp_path)) == 1
    assert hive.run(START + 240)['sent'] == 1
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [text, text]
    sent = api.get(hive.conn, cid)['reminder_state']
    assert (sent['state'], sent['attempts'], sent['receipt']) == ('delivered', 1, receipt(102))
    assert len(hive.events('reminder_fallback')) == 1


def test_a_second_process_run_sends_nothing_twice(hive, tmp_path):
    cid, _ = alert(hive)
    other = store.connect(hive.cfg['db'])
    assert hive.run(START, conn=other)['fired'] == []
    assert hive.run(START + 180)['sent'] == 1
    for at in (START + 180, START + 1800):
        summary = hive.run(at, conn=store.connect(hive.cfg['db']))
        assert (summary['sent'], summary['fired'], summary['published']) == (0, [], 0)
    assert len(tg_calls(tmp_path)) == 1 and len(hive.events('watchdog_alert')) == 1
    assert len(hive.events('reminder_fallback')) == 1 and api.get(hive.conn, cid)['state'] == 'done'


def test_a_dispatcher_stuck_holding_its_lock_does_not_stop_the_fallback(hive, tmp_path):
    cid, text = alert(hive)
    hive.section['lock_wait_s'] = 3
    with Path(hive.cfg['db']).with_name('dispatcher.lock').open('a') as held:
        fcntl.flock(held, fcntl.LOCK_EX)
        summary = hive.run(START + 180)
    assert (summary['lock'], summary['sent']) == ('timeout', 1)
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [text]
    assert api.get(hive.conn, cid)['reminder_state']['state'] == 'delivered'


def test_a_ticking_dispatcher_sends_an_overdue_alert_too(hive, env, tmp_path):
    _, _, d = env
    cid, text = alert(hive)
    d.cfg = hive.cfg
    d.clock = lambda: hive.now[0]
    hive.now[0] = START + 180
    d.tick()
    assert [c['stdin'] for c in tg_calls(tmp_path)] == [text]
    assert api.get(hive.conn, cid)['reminder_state']['sent_by'] == 'dispatcher'
    assert hive.run(START + 300)['sent'] == 0 and len(tg_calls(tmp_path)) == 1


def test_alerts_are_valid_c1_packets_for_the_owner_inbox(hive, tmp_path, c1):
    cfg = dict(hive.cfg, delivery='ulei', **ulei_keys(tmp_path, c1.path))
    cooldown(hive.conn)
    [fired] = hive.run(START, cfg=cfg)['fired']
    hive.run(START + 180, cfg=cfg)
    cid = fired['card']
    assert [(p['kind'], p['id']) for p in c1.published()] == [
        ('watchdog_alert', f'hive:{cid}:watchdog_alert'), ('reminder_fallback', f'hive:{cid}:reminder_fallback')]


def test_an_inbox_refusing_the_kind_keeps_the_event_and_the_fallback_still_sends(hive, env, tmp_path):
    cfg, _, _ = env
    cooldown(hive.conn)
    summary = hive.run(START, cfg=dict(cfg, delivery='drop'))
    assert len(summary['fired']) == 1 and summary['published'] == 0
    cid = summary['fired'][0]['card']
    row = hive.conn.execute('SELECT published_at, error FROM outbox WHERE event_id=?',
                            (f'hive:{cid}:watchdog_alert',)).fetchone()
    assert row['published_at'] is None and row['error'] == 'ValueError: invalid packet values'
    assert hive.run(START + 180, cfg=dict(cfg, delivery='drop'))['sent'] == 1
    assert api.get(hive.conn, cid)['reminder_state']['state'] == 'delivered' and len(tg_calls(tmp_path)) == 1


def test_a_failed_source_is_an_error_and_the_other_conditions_still_fire(hive):
    cooldown(hive.conn)
    hive.section.update(escalations_command=hive.source('escalations', 'not json'), bridge_unit='synthetic.service',
                        journal_command=hive.source('journal', code=1))
    summary = hive.run(START)
    assert [f['condition'] for f in summary['fired']] == ['vendor_cooldown']
    assert summary['errors'] == ["escalation_unread: escalations_command: not JSON: Expecting value: line 1 column 1 "
                                 "(char 0)", 'bridge_token_refused: journal_command: exit 1']


def test_cli_runs_one_pass_and_prints_its_summary(hive, capsys):
    cooldown(hive.conn)
    code, out, err = cli(capsys, 'watchdog')
    summary = json.loads(out)
    assert (code, err, summary['errors'], summary['lock']) == (0, '', [], 'held')
    assert [f['key'] for f in summary['fired']] == ['vendor_cooldown:claude']
    code, out, _ = cli(capsys, 'watchdog')
    assert code == 0 and json.loads(out)['fired'] == []


@pytest.mark.parametrize('table,error', [
    ({'stall_s': 0}, 'config: watchdog.stall_s must be a positive number, got 0$'),
    ({'nudge_repeats': 2.5}, 'config: watchdog.nudge_repeats must be a positive integer, got 2.5$'),
    ({'qb_usage_pct': 101}, r'config: watchdog.qb_usage_pct must be a number in \(0, 100\], got 101$'),
    ({'bridge_pattern': '('}, "config: watchdog.bridge_pattern must be a regular expression, got '\\('$"),
    ({'escalations_command': 'escalations'}, 'config: watchdog.escalations_command must be a non-empty list'),
    ({'cooldown_s': {'wait_nudge_repeat': 60}}, 'config: watchdog.cooldown_s must be a table of queue_stalled'),
    ({'stal_s': 900}, 'config: unknown key watchdog.stal_s$'),
])
def test_watchdog_table_is_checked(table, error):
    with pytest.raises(ValueError, match=error):
        watchdog.config({'watchdog': table})
